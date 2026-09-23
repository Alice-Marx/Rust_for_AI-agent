//! Bounded, direct stream-json transport for an unmodified official Claude CLI.
//! This is a versioned protocol integration, not a remote-model attestation.
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

// New versions must pass the offline protocol fixture before extending this list.
const SUPPORTED_VERSION: &str = "2.1.193";
const READ_TOOLS: &[&str] = &["Read", "Glob", "Grep", "AskUserQuestion"];
const WRITE_TOOLS: &[&str] = &["Bash", "Edit", "Write", "NotebookEdit"];

pub(super) fn validate(req: &NativeRequest) -> Result<()> {
    ensure!(req.app_id == "claude", "official Claude profile required");
    ensure!(
        req.model.starts_with("claude-")
            && req.model.len() <= 160
            && req
                .model
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.'),
        "Claude requires a full, explicit claude-* model ID; aliases are unsupported"
    );
    ensure!(
        req.config_path.is_none(),
        "Claude uses its official account; custom configuration injection is unsupported"
    );
    if let Some(effort) = &req.reasoning_effort {
        ensure!(
            ["low", "medium", "high", "xhigh", "max"].contains(&effort.as_str()),
            "unsupported Claude reasoning effort"
        );
    }
    Ok(())
}

struct Prepared {
    executable: PathBuf,
    sha256: String,
}

fn read_json(path: &Path) -> Result<Value> {
    use std::io::Read;
    let file = std::fs::File::open(path).context("cannot read Claude package metadata")?;
    let mut data = Vec::new();
    file.take(64 * 1024 + 1).read_to_end(&mut data)?;
    ensure!(data.len() <= 64 * 1024, "Claude package metadata too large");
    serde_json::from_slice(&data).context("invalid Claude package metadata")
}

fn prepare() -> Result<Prepared> {
    let resolved = crate::desktop_bridge::find_executable("claude")
        .context("official Claude CLI is not installed; install @anthropic-ai/claude-code")?;
    let canonical = std::fs::canonicalize(&resolved)?;
    let mut roots = Vec::new();
    for path in [&resolved, &canonical] {
        if let Some(parent) = path.parent() {
            roots.push(parent.join("node_modules/@anthropic-ai/claude-code"));
            roots.push(parent.join("../lib/node_modules/@anthropic-ai/claude-code"));
            for ancestor in parent.ancestors().take(4) {
                if ancestor
                    .file_name()
                    .is_some_and(|name| name == "claude-code")
                {
                    roots.push(ancestor.to_path_buf());
                }
            }
        }
    }
    for root in roots {
        let metadata = root.join("package.json");
        if !metadata.is_file() {
            continue;
        }
        let package = read_json(&metadata)?;
        ensure!(
            package["name"] == "@anthropic-ai/claude-code"
                && package["homepage"] == "https://github.com/anthropics/claude-code",
            "Claude package identity is not the official npm package"
        );
        ensure!(
            package["version"] == SUPPORTED_VERSION,
            "Claude CLI version is unverified; supported stream-json version is {SUPPORTED_VERSION}; use the manual terminal until adapter validation"
        );
        let entry = package
            .pointer("/bin/claude")
            .and_then(Value::as_str)
            .context("official Claude native package entry missing")?;
        ensure!(
            matches!(entry, "bin/claude.exe" | "bin/claude"),
            "only the official native Claude npm entry is supported"
        );
        let executable = std::fs::canonicalize(root.join(entry))?;
        let root = std::fs::canonicalize(&root)?;
        ensure!(
            executable.starts_with(&root),
            "Claude package executable escapes package root"
        );
        let profile = crate::desktop_bridge::CliProfile {
            id: "claude".into(),
            name: "Claude Code".into(),
            executable: executable.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let status = crate::desktop_bridge::detect_cli(&profile);
        ensure!(
            status.version.as_deref() == Some("2.1.193 (Claude Code)"),
            "Claude executable version banner is unavailable or unsupported"
        );
        return Ok(Prepared {
            sha256: executable_digest(&executable)?,
            executable,
        });
    }
    bail!("Claude native npm package metadata is unavailable; standalone binaries and local forks require the manual terminal")
}

fn settings() -> Value {
    json!({
        "disableAllHooks": true,
        "enabledPlugins": {},
        "autoMemoryEnabled": false,
        "fallbackModel": [],
        "permissions": {"defaultMode": "default", "ask": WRITE_TOOLS}
    })
}

fn arguments(req: &NativeRequest, session: &str) -> Vec<String> {
    let mut tools = READ_TOOLS.to_vec();
    if !req.read_only {
        tools.extend_from_slice(WRITE_TOOLS);
    }
    let mut args: Vec<String> = [
        "--print",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--safe-mode",
        "--model",
        &req.model,
        "--tools",
        &tools.join(","),
        "--permission-prompt-tool",
        "stdio",
        "--permission-mode",
        "default",
        "--setting-sources",
        "",
        "--settings",
        &settings().to_string(),
        "--strict-mcp-config",
        "--mcp-config",
        "{\"mcpServers\":{}}",
        "--disable-slash-commands",
        "--no-chrome",
        "--no-session-persistence",
        "--session-id",
        session,
        "--fallback-model",
        "",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    if let Some(effort) = &req.reasoning_effort {
        args.extend(["--effort".into(), effort.clone()]);
    }
    args
}

fn clean_environment(command: &mut tokio::process::Command) {
    // Keep official OAuth/API credentials in the child's own environment; never
    // inspect or log their values. All noncredential Claude routing/customization
    // overrides are removed; account storage is read exclusively by official CLI.
    for (name, _) in std::env::vars_os() {
        let upper = name.to_string_lossy().to_ascii_uppercase();
        if (upper.starts_with("CLAUDE_") || upper.starts_with("ANTHROPIC_"))
            && !matches!(
                upper.as_str(),
                "ANTHROPIC_API_KEY"
                    | "ANTHROPIC_AUTH_TOKEN"
                    | "CLAUDE_CODE_OAUTH_TOKEN"
                    | "CLAUDE_CONFIG_DIR"
            )
        {
            command.env_remove(name);
        }
    }
    for name in [
        "NODE_OPTIONS",
        "BUN_OPTIONS",
        "NODE_EXTRA_CA_CERTS",
        "USER_TYPE",
    ] {
        command.env_remove(name);
    }
    command
        .env("CLAUDE_CODE_SAFE_MODE", "1")
        .env("CLAUDE_CODE_DISABLE_REFUSAL_FALLBACK", "1")
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env("CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL", "1")
        .env("DISABLE_AUTOUPDATER", "1");
}

pub(super) async fn execute(
    req: NativeRequest,
    events: mpsc::Sender<NativeEvent>,
    mut cancel: watch::Receiver<bool>,
    controls: mpsc::Receiver<NativeControl>,
) -> Result<NativeResult> {
    validate(&req)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(req.max_duration_secs);
    if *cancel.borrow() {
        return Ok(empty_result(&req, "cancelled"));
    }
    let preparation = tokio::task::spawn_blocking(prepare);
    let prepared = tokio::select! {
        result = preparation => result??,
        _ = cancellation(&mut cancel) => return Ok(empty_result(&req, "cancelled")),
        _ = tokio::time::sleep_until(deadline) => return Ok(empty_result(&req, "timed_out")),
        _ = events.closed() => bail!("native event consumer disconnected during preparation"),
    };
    emit(&events, NativeEvent::Identity {
        executable: prepared.executable.to_string_lossy().into_owned(),
        version: SUPPORTED_VERSION.into(), sha256: prepared.sha256.clone(),
        verification: "Official npm metadata and native entry checked; file digest recorded. Publisher signature and remote model identity are not attested".into(),
    }).await?;
    let session = uuid::Uuid::new_v4().to_string();
    let mut command = tokio::process::Command::new(&prepared.executable);
    command
        .args(arguments(&req, &session))
        .current_dir(&req.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    clean_environment(&mut command);
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .context("could not start official Claude CLI")?;
    let tree = match crate::process_tree::ProcessTree::attach(
        child.id().context("missing Claude process ID")?,
    ) {
        Ok(tree) => tree,
        Err(error) => {
            let _ = child.kill().await;
            return Err(error.context("cannot contain Claude process tree"));
        }
    };
    let stdin = child.stdin.take().context("Claude stdin unavailable")?;
    let stdout = child.stdout.take().context("Claude stdout unavailable")?;
    let (frames_tx, frames) = mpsc::channel(64);
    let reader = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut frame = Vec::new();
            let result = match read_frame(&mut reader, &mut frame).await {
                Ok(true) => serde_json::from_slice(&frame).context("invalid Claude JSON frame"),
                Ok(false) => Err(anyhow::anyhow!(
                    "Claude closed its protocol before a terminal result"
                )),
                Err(error) => Err(error),
            };
            let failed = result.is_err();
            if frames_tx.send(result).await.is_err() || failed {
                break;
            }
        }
    });
    let mut runner = Runner::new(
        req.clone(),
        session,
        Box::new(stdin),
        frames,
        controls,
        events.clone(),
    );
    runner.set_executable_identity(SUPPORTED_VERSION, &prepared.sha256);
    let outcome = tokio::select! {
        result = runner.run() => result,
        _ = cancellation(&mut cancel) => Ok("cancelled".to_owned()),
        _ = tokio::time::sleep_until(deadline) => Ok("timed_out".to_owned()),
        _ = events.closed() => Err(anyhow::anyhow!("native event consumer disconnected")),
    };
    if !matches!(&outcome, Ok(status) if status == "completed") {
        let _ = tokio::time::timeout(Duration::from_millis(500), runner.write(json!({
            "type":"control_request", "request_id":"host-interrupt", "request":{"subtype":"interrupt"}
        }))).await;
    }
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
    input: Value,
    tool_id: String,
    question: bool,
}

#[derive(Default)]
struct MessageState {
    text: String,
    reasoning: String,
    complete: bool,
    block: Option<BlockState>,
}

struct BlockState {
    index: u64,
    kind: String,
    text: String,
    ended: bool,
}

struct Runner {
    req: NativeRequest,
    session: String,
    stdin: Box<dyn AsyncWrite + Send + Unpin>,
    frames: mpsc::Receiver<Result<Value>>,
    controls: mpsc::Receiver<NativeControl>,
    controls_open: bool,
    events: mpsc::Sender<NativeEvent>,
    result: NativeResult,
    pending: HashMap<String, Pending>,
    control_ids: HashSet<String>,
    messages: HashMap<String, MessageState>,
    active_message: Option<String>,
    init_seen: bool,
    output_bytes: usize,
}

impl Runner {
    fn new(
        req: NativeRequest,
        session: String,
        stdin: Box<dyn AsyncWrite + Send + Unpin>,
        frames: mpsc::Receiver<Result<Value>>,
        controls: mpsc::Receiver<NativeControl>,
        events: mpsc::Sender<NativeEvent>,
    ) -> Self {
        Self {
            result: empty_result(&req, "completed"),
            req,
            session,
            stdin,
            frames,
            controls,
            controls_open: true,
            events,
            pending: HashMap::new(),
            control_ids: HashSet::new(),
            messages: HashMap::new(),
            active_message: None,
            init_seen: false,
            output_bytes: 0,
        }
    }

    fn set_executable_identity(&mut self, version: &str, sha256: &str) {
        self.result.identity.tool_version = Some(version.to_owned());
        self.result.identity.executable_sha256 = Some(sha256.to_owned());
        super::record_identity_evidence(&mut self.result.identity, "local_executable_probe");
    }

    async fn write(&mut self, value: Value) -> Result<()> {
        let mut data = serde_json::to_vec(&value)?;
        ensure!(
            data.len() < MAX_FRAME_BYTES,
            "Claude input frame exceeds limit"
        );
        data.push(b'\n');
        self.stdin.write_all(&data).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn rpc(&mut self, id: &str, request: Value) -> Result<Value> {
        self.write(json!({"type":"control_request", "request_id":id, "request":request}))
            .await?;
        loop {
            let value = self
                .frames
                .recv()
                .await
                .context("Claude protocol reader closed")??;
            match value["type"].as_str() {
                Some("control_response") => {
                    let response = &value["response"];
                    ensure!(
                        response["request_id"] == id,
                        "unexpected Claude handshake response ID"
                    );
                    ensure!(
                        response["subtype"] == "success",
                        "Claude does not support required handshake capability"
                    );
                    return response
                        .get("response")
                        .cloned()
                        .context("missing Claude handshake payload");
                }
                Some("control_request") => {
                    let id = required_string(&value, "request_id")?;
                    self.reply(
                        id,
                        json!({"behavior":"deny", "message":"Host has not validated this session"}),
                    )
                    .await?;
                    bail!("Claude attempted a tool or customization before session validation")
                }
                Some("keep_alive") => {}
                Some("system") if value["subtype"] == "init" => self.init(&value).await?,
                _ => bail!("unexpected Claude frame before prompt submission"),
            }
        }
    }

    async fn run(&mut self) -> Result<String> {
        let initialize = self
            .rpc(
                "host-initialize",
                json!({"subtype":"initialize", "hooks":{}, "agents":{},
            "sdkMcpServers":[], "promptSuggestions":false, "agentProgressSummaries":false}),
            )
            .await?;
        ensure!(
            initialize
                .pointer("/account/apiProvider")
                .and_then(Value::as_str)
                == Some("firstParty"),
            "Claude account provider is unknown or rerouted; native adapter requires firstParty"
        );
        // The official CLI reports this only as firstParty. It establishes the
        // Anthropic provider boundary, but does not distinguish API billing from
        // a Claude subscription.
        self.result.identity.effective_provider = Some("anthropic".into());
        super::record_identity_evidence(
            &mut self.result.identity,
            "claude_first_party_account_provider",
        );
        let applied = self
            .rpc("host-settings", json!({"subtype":"get_settings"}))
            .await?;
        validate_settings(&applied, &self.req)?;
        self.result.identity.configured_model = applied
            .pointer("/applied/model")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let applied_effort = applied
            .pointer("/applied/effort")
            .and_then(Value::as_str)
            .filter(|effort| ["low", "medium", "high", "xhigh", "max"].contains(effort))
            .map(str::to_owned);
        self.result.identity.configured_reasoning_effort = applied_effort.clone();
        if self.req.reasoning_effort.is_none()
            || applied_effort.as_deref() == self.req.reasoning_effort.as_deref()
        {
            self.result.identity.effective_reasoning_effort = applied_effort;
        }
        super::record_identity_evidence(&mut self.result.identity, "claude_applied_settings");
        let mcp = self
            .rpc("host-mcp", json!({"subtype":"mcp_status"}))
            .await?;
        ensure!(
            mcp["mcpServers"].as_array().is_some_and(Vec::is_empty),
            "Claude MCP boundary is not empty"
        );
        emit(
            &self.events,
            NativeEvent::Started {
                app_id: "claude".into(),
                model: self.req.model.clone(),
                protocol: "claude-stream-json/2.1.193".into(),
            },
        )
        .await?;
        self.result.session_id = Some(self.session.clone());
        emit(
            &self.events,
            NativeEvent::SessionStarted {
                session_id: self.session.clone(),
            },
        )
        .await?;
        self.write(json!({"type":"user", "message":{"role":"user", "content":self.req.prompt},
            "parent_tool_use_id":null, "session_id":self.session, "uuid":uuid::Uuid::new_v4().to_string()})).await?;
        loop {
            tokio::select! {
                frame = self.frames.recv() => {
                    let frame = frame.context("Claude protocol reader closed")??;
                    if self.frame(frame).await? { return Ok("completed".into()); }
                }
                control = self.controls.recv(), if self.controls_open => {
                    match control {
                        Some(control) => self.control(control).await?,
                        None => {
                            self.controls_open = false;
                            let ids: Vec<_> = self.pending.keys().cloned().collect();
                            for id in ids { self.deny(&id, "Host approval channel closed").await?; }
                        }
                    }
                }
            }
        }
    }

    fn check_session(&self, value: &Value) -> Result<()> {
        ensure!(
            value["session_id"].as_str() == Some(&self.session),
            "Claude session identity changed or missing"
        );
        ensure!(
            value.get("parent_tool_use_id").is_none_or(Value::is_null),
            "Claude subagent output is outside the model boundary"
        );
        Ok(())
    }

    fn check_model(&self, model: &Value) -> Result<()> {
        ensure!(
            model.as_str() == Some(&self.req.model),
            "Claude reported a different or missing model; no fallback is allowed"
        );
        Ok(())
    }

    async fn init(&mut self, value: &Value) -> Result<()> {
        self.check_session(value)?;
        self.check_model(&value["model"])?;
        ensure!(!self.init_seen, "duplicate Claude session initialization");
        ensure!(
            value["claude_code_version"] == SUPPORTED_VERSION,
            "Claude executable version differs from verified npm metadata"
        );
        ensure!(
            value["permissionMode"] == "default",
            "Claude permission mode changed"
        );
        for key in ["mcp_servers", "plugins", "skills"] {
            ensure!(
                value[key].as_array().is_some_and(Vec::is_empty),
                "Claude customization boundary is not empty: {key}"
            );
        }
        let tools = value["tools"]
            .as_array()
            .context("Claude tool inventory missing")?;
        for tool in tools {
            ensure!(
                self.tool_allowed(tool.as_str().unwrap_or("")),
                "Claude exposed a tool outside the allowed set"
            );
        }
        self.init_seen = true;
        Ok(())
    }

    fn tool_allowed(&self, name: &str) -> bool {
        READ_TOOLS.contains(&name) || (!self.req.read_only && WRITE_TOOLS.contains(&name))
    }

    async fn frame(&mut self, value: Value) -> Result<bool> {
        match value["type"].as_str() {
            Some("system") if value["subtype"] == "init" => self.init(&value).await?,
            Some("system") => {
                self.check_session(&value)?;
                if let Some(mode) = value.get("permissionMode") {
                    ensure!(mode == "default", "Claude permission mode changed");
                }
                if let Some(model) = value.get("model") {
                    self.check_model(model)?;
                }
                if value["subtype"] == "status" || value["subtype"] == "compact_boundary" {
                    emit(
                        &self.events,
                        NativeEvent::Status {
                            message: format!(
                                "Claude: {}",
                                value["subtype"].as_str().unwrap_or("status")
                            ),
                        },
                    )
                    .await?;
                }
            }
            Some("stream_event") => self.stream(&value).await?,
            Some("assistant") => self.assistant(&value).await?,
            Some("control_request") => self.permission(&value).await?,
            Some("control_cancel_request") => {
                let id = required_string(&value, "request_id")?;
                if self.pending.remove(id).is_some() {
                    emit(
                        &self.events,
                        NativeEvent::PermissionResolved {
                            request_id: id.into(),
                            approved: false,
                        },
                    )
                    .await?;
                }
            }
            Some("result") => {
                self.check_session(&value)?;
                ensure!(
                    self.init_seen,
                    "Claude terminal result preceded session validation"
                );
                ensure!(
                    value["subtype"] == "success" && value["is_error"] == false,
                    "Claude execution failed or stopped at a limit"
                );
                if let Some(reason) = value.get("terminal_reason").and_then(Value::as_str) {
                    ensure!(
                        reason == "completed",
                        "Claude reported an incomplete terminal reason"
                    );
                }
                let usage = value["modelUsage"]
                    .as_object()
                    .context("Claude result model usage missing")?;
                ensure!(
                    !usage.is_empty() && usage.keys().all(|model| model == &self.req.model),
                    "Claude result used an unrequested model or did not report model usage"
                );
                self.result.identity.effective_model = Some(self.req.model.clone());
                super::record_identity_evidence(&mut self.result.identity, "claude_model_usage");
                ensure!(
                    self.pending.is_empty(),
                    "Claude completed while approval was pending"
                );
                ensure!(
                    !self.messages.is_empty(),
                    "Claude completed without model-attributed assistant output"
                );
                // Each streamed content block is followed by an assistant
                // snapshot, but that snapshot does not end the API message.
                // The verified CLI sends content_block_stop and message_stop
                // before its success result; a truncated stream is not success.
                ensure!(
                    self.active_message.is_none()
                        && self.messages.values().all(|message| {
                            message.complete
                                && message.block.as_ref().is_none_or(|block| block.ended)
                        }),
                    "Claude completed with an unfinished assistant stream"
                );
                if let Some(text) = value["result"].as_str().filter(|text| !text.is_empty()) {
                    ensure!(
                        self.result.output.ends_with(text),
                        "Claude final result does not match streamed assistant output"
                    );
                }
                let data = json!({"usage":value["usage"], "modelUsage":value["modelUsage"], "total_cost_usd":value["total_cost_usd"]});
                self.result.usage = Some(data.clone());
                emit(&self.events, NativeEvent::Usage { data }).await?;
                return Ok(true);
            }
            Some("user") => {
                self.check_session(&value)?;
                emit(&self.events, NativeEvent::ToolActivity { data:json!({"type":"tool_result", "content":value.pointer("/message/content")}) }).await?;
            }
            Some("tool_progress" | "tool_use_summary") => {
                self.check_session(&value)?;
                if let Some(tool) = value.get("tool_name").and_then(Value::as_str) {
                    ensure!(self.tool_allowed(tool), "unexpected Claude tool activity");
                }
                emit(&self.events, NativeEvent::ToolActivity { data: value }).await?;
            }
            Some("keep_alive" | "rate_limit_event") => {}
            _ => bail!("unsupported Claude protocol frame"),
        }
        Ok(false)
    }

    async fn append(&mut self, id: &str, text: &str, reasoning: bool) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        self.output_bytes = self
            .output_bytes
            .checked_add(text.len())
            .context("Claude output limit exceeded")?;
        ensure!(
            self.output_bytes <= MAX_OUTPUT_BYTES,
            "Claude output limit exceeded"
        );
        let state = self
            .messages
            .get_mut(id)
            .context("Claude delta has no model-attributed message")?;
        ensure!(
            !state.complete,
            "Claude delta arrived after completed message"
        );
        if reasoning {
            state.reasoning.push_str(text);
            emit(
                &self.events,
                NativeEvent::ReasoningDelta { text: text.into() },
            )
            .await?;
        } else {
            state.text.push_str(text);
            self.result.output.push_str(text);
            emit(&self.events, NativeEvent::TextDelta { text: text.into() }).await?;
        }
        Ok(())
    }

    async fn stream(&mut self, value: &Value) -> Result<()> {
        self.check_session(value)?;
        ensure!(self.init_seen, "Claude stream preceded session validation");
        let event = &value["event"];
        match event["type"].as_str() {
            Some("message_start") => {
                self.check_model(&event["message"]["model"])?;
                let id = required_string(&event["message"], "id")?.to_owned();
                ensure!(
                    self.messages.len() < 4096 && !self.messages.contains_key(&id),
                    "duplicate or excessive Claude messages"
                );
                self.messages.insert(id.clone(), MessageState::default());
                self.active_message = Some(id);
            }
            Some("content_block_start") => {
                let block = &event["content_block"];
                let id = self
                    .active_message
                    .clone()
                    .context("Claude block lacks message_start")?;
                let index = event["index"]
                    .as_u64()
                    .context("Claude block index missing")?;
                let kind = required_string(block, "type")?;
                ensure!(
                    matches!(kind, "text" | "thinking" | "redacted_thinking" | "tool_use"),
                    "unsupported Claude content block"
                );
                let previous = &self.messages[&id].block;
                ensure!(
                    index < 4096
                        && previous
                            .as_ref()
                            .is_none_or(|previous| previous.ended && index > previous.index),
                    "invalid Claude content block order"
                );
                self.messages.get_mut(&id).expect("message exists").block = Some(BlockState {
                    index,
                    kind: kind.into(),
                    text: String::new(),
                    ended: false,
                });
                if matches!(kind, "text" | "thinking") {
                    let initial = required_string_allow_empty(
                        block,
                        if kind == "text" { "text" } else { "thinking" },
                    )?;
                    self.append(&id, initial, kind == "thinking").await?;
                    self.messages
                        .get_mut(&id)
                        .expect("message exists")
                        .block
                        .as_mut()
                        .expect("block exists")
                        .text = initial.into();
                }
                if block["type"] == "tool_use" {
                    ensure!(
                        self.tool_allowed(required_string(block, "name")?),
                        "Claude attempted an unapproved tool"
                    );
                    emit(
                        &self.events,
                        NativeEvent::ToolActivity {
                            data: block.clone(),
                        },
                    )
                    .await?;
                }
            }
            Some("content_block_delta") => {
                let id = self
                    .active_message
                    .clone()
                    .context("Claude delta lacks message_start")?;
                let delta = &event["delta"];
                let block = self
                    .messages
                    .get_mut(&id)
                    .context("Claude delta has unknown message")?
                    .block
                    .as_mut()
                    .context("Claude delta lacks content_block_start")?;
                ensure!(
                    !block.ended && event["index"].as_u64() == Some(block.index),
                    "Claude delta has invalid block index or follows block end"
                );
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        ensure!(block.kind == "text", "Claude text delta has non-text block");
                        let text = required_string_allow_empty(delta, "text")?;
                        block.text.push_str(text);
                        self.append(&id, required_string_allow_empty(delta, "text")?, false)
                            .await?
                    }
                    Some("thinking_delta") => {
                        ensure!(
                            block.kind == "thinking",
                            "Claude thinking delta has non-thinking block"
                        );
                        let text = required_string_allow_empty(delta, "thinking")?;
                        block.text.push_str(text);
                        self.append(&id, required_string_allow_empty(delta, "thinking")?, true)
                            .await?
                    }
                    Some("signature_delta") => ensure!(
                        block.kind == "thinking",
                        "Claude signature has non-thinking block"
                    ),
                    Some("input_json_delta") => ensure!(
                        block.kind == "tool_use",
                        "Claude tool delta has non-tool block"
                    ),
                    _ => bail!("unsupported Claude content delta"),
                }
            }
            Some("content_block_stop") => {
                let id = self
                    .active_message
                    .as_ref()
                    .context("Claude block stop lacks active message")?;
                let block = self
                    .messages
                    .get_mut(id)
                    .context("Claude block stop has unknown message")?
                    .block
                    .as_mut()
                    .context("Claude block stop lacks block")?;
                ensure!(
                    event["index"].as_u64() == Some(block.index) && !block.ended,
                    "Claude stopped an unknown or ended block"
                );
                block.ended = true;
            }
            Some("message_stop") => {
                let id = self
                    .active_message
                    .take()
                    .context("Claude message stop lacks active message")?;
                self.messages
                    .get_mut(&id)
                    .context("Claude stopped unknown message")?
                    .complete = true;
            }
            Some("message_delta") => {}
            _ => bail!("unsupported Claude streaming event"),
        }
        Ok(())
    }

    async fn assistant(&mut self, value: &Value) -> Result<()> {
        self.check_session(value)?;
        ensure!(
            self.init_seen,
            "Claude assistant preceded session validation"
        );
        ensure!(
            value.get("error").is_none_or(Value::is_null),
            "Claude assistant reported a provider error"
        );
        let message = &value["message"];
        self.check_model(&message["model"])?;
        let id = required_string(message, "id")?.to_owned();
        ensure!(
            self.messages.contains_key(&id) || self.messages.len() < 4096,
            "excessive Claude messages"
        );
        self.messages.entry(id.clone()).or_default();
        let blocks = message["content"]
            .as_array()
            .context("Claude assistant content missing")?;
        // The official CLI emits one assistant snapshot per content block,
        // before content_block_stop, reusing the API message ID. Treating the
        // thinking snapshot as a complete message would lose subsequent text.
        if let Some(streamed) = self.messages[&id].block.as_ref() {
            ensure!(
                blocks.len() == 1,
                "Claude streamed assistant snapshot must contain exactly one block"
            );
            let block = &blocks[0];
            ensure!(
                block["type"].as_str() == Some(&streamed.kind),
                "Claude snapshot changed block type"
            );
            match streamed.kind.as_str() {
                "text" | "thinking" => {
                    let reasoning = streamed.kind == "thinking";
                    let text = required_string_allow_empty(
                        block,
                        if reasoning { "thinking" } else { "text" },
                    )?;
                    ensure!(
                        text.starts_with(&streamed.text),
                        "Claude block snapshot disagrees with streamed content"
                    );
                    let suffix = text[streamed.text.len()..].to_owned();
                    self.append(&id, &suffix, reasoning).await?;
                    self.messages
                        .get_mut(&id)
                        .expect("message exists")
                        .block
                        .as_mut()
                        .expect("block exists")
                        .text = text.into();
                }
                "tool_use" => {
                    ensure!(
                        self.tool_allowed(required_string(block, "name")?),
                        "Claude attempted an unapproved tool"
                    );
                    emit(
                        &self.events,
                        NativeEvent::ToolActivity {
                            data: block.clone(),
                        },
                    )
                    .await?;
                }
                "redacted_thinking" => {}
                _ => bail!("unsupported Claude assistant block"),
            }
            return Ok(());
        }
        let mut text = String::new();
        let mut reasoning = String::new();
        for block in blocks {
            match block["type"].as_str() {
                Some("text") => text.push_str(required_string_allow_empty(block, "text")?),
                Some("thinking") => {
                    reasoning.push_str(required_string_allow_empty(block, "thinking")?)
                }
                Some("tool_use") => {
                    ensure!(
                        self.tool_allowed(required_string(block, "name")?),
                        "Claude attempted an unapproved tool"
                    );
                    emit(
                        &self.events,
                        NativeEvent::ToolActivity {
                            data: block.clone(),
                        },
                    )
                    .await?;
                }
                Some("redacted_thinking") => {}
                _ => bail!("unsupported Claude assistant content"),
            }
        }
        let state = &self.messages[&id];
        ensure!(
            text.starts_with(&state.text) && reasoning.starts_with(&state.reasoning),
            "Claude full assistant disagrees with streamed content"
        );
        let suffix = text[state.text.len()..].to_owned();
        let reasoning_suffix = reasoning[state.reasoning.len()..].to_owned();
        self.append(&id, &suffix, false).await?;
        self.append(&id, &reasoning_suffix, true).await?;
        self.messages.get_mut(&id).expect("message exists").complete = true;
        Ok(())
    }

    async fn reply(&mut self, id: &str, response: Value) -> Result<()> {
        self.write(json!({"type":"control_response", "response":{"subtype":"success", "request_id":id, "response":response}})).await
    }

    async fn deny(&mut self, id: &str, reason: &str) -> Result<()> {
        if let Some(pending) = self.pending.remove(id) {
            self.reply(
                id,
                json!({"behavior":"deny", "message":reason, "toolUseID":pending.tool_id}),
            )
            .await?;
            emit(
                &self.events,
                NativeEvent::PermissionResolved {
                    request_id: id.into(),
                    approved: false,
                },
            )
            .await?;
        }
        Ok(())
    }

    async fn permission(&mut self, value: &Value) -> Result<()> {
        let id = required_string(value, "request_id")?.to_owned();
        ensure!(
            id.len() <= 256 && self.control_ids.len() < 4096 && self.control_ids.insert(id.clone()),
            "duplicate or excessive Claude control requests"
        );
        let request = &value["request"];
        ensure!(
            request["subtype"] == "can_use_tool",
            "unsupported Claude control request"
        );
        let tool = required_string(request, "tool_name")?;
        let tool_id = required_string(request, "tool_use_id")?.to_owned();
        ensure!(
            request["input"].is_object(),
            "Claude permission input is not an object"
        );
        if !self.init_seen
            || !self.tool_allowed(tool)
            || request.get("agent_id").is_some_and(|v| !v.is_null())
        {
            self.reply(&id, json!({"behavior":"deny", "message":"Tool is outside the host execution boundary", "toolUseID":tool_id})).await?;
            bail!("Claude requested a tool outside the verified boundary")
        }
        ensure!(
            self.pending.len() < 64,
            "too many pending Claude permissions"
        );
        let question = tool == "AskUserQuestion";
        let input = request["input"].clone();
        let question_data = if question {
            Some(normalize_questions(&input)?)
        } else {
            None
        };
        self.pending.insert(
            id.clone(),
            Pending {
                input: input.clone(),
                tool_id,
                question,
            },
        );
        if !self.controls_open || self.controls.is_closed() {
            self.deny(&id, "No host approval handler is available")
                .await?;
        } else if let Some(data) = question_data {
            emit(
                &self.events,
                NativeEvent::QuestionRequested {
                    request_id: id,
                    data,
                },
            )
            .await?;
        } else {
            emit(
                &self.events,
                NativeEvent::PermissionRequested {
                    request_id: id,
                    description: format!("Claude requests {tool}"),
                    data: json!({"tool":tool, "input":input}),
                },
            )
            .await?;
        }
        Ok(())
    }

    async fn control(&mut self, control: NativeControl) -> Result<()> {
        match control {
            NativeControl::Permission {
                request_id,
                approve,
            } => {
                let Some(pending) = self.pending.get(&request_id) else {
                    return Ok(());
                };
                if !approve {
                    return self.deny(&request_id, "User denied this request").await;
                }
                ensure!(
                    !pending.question,
                    "Claude question requires answers, not a permission grant"
                );
                let pending = self
                    .pending
                    .remove(&request_id)
                    .expect("pending request exists");
                self.reply(
                    &request_id,
                    json!({"behavior":"allow", "updatedInput":pending.input,
                    "toolUseID":pending.tool_id, "decisionClassification":"user_temporary"}),
                )
                .await?;
                emit(
                    &self.events,
                    NativeEvent::PermissionResolved {
                        request_id,
                        approved: true,
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
                ensure!(
                    pending.question,
                    "Claude permission request cannot be answered as a question"
                );
                validate_answers(&pending.input, &answers)?;
                let mut pending = self
                    .pending
                    .remove(&request_id)
                    .expect("pending question exists");
                pending.input["answers"] = answers;
                self.reply(
                    &request_id,
                    json!({"behavior":"allow", "updatedInput":pending.input,
                    "toolUseID":pending.tool_id, "decisionClassification":"user_temporary"}),
                )
                .await?;
                emit(
                    &self.events,
                    NativeEvent::PermissionResolved {
                        request_id,
                        approved: true,
                    },
                )
                .await?;
            }
        }
        Ok(())
    }
}

fn required_string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    let text = required_string_allow_empty(value, key)?;
    ensure!(!text.is_empty(), "Claude protocol string is empty: {key}");
    Ok(text)
}

fn required_string_allow_empty<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("Claude protocol string missing: {key}"))
}

fn validate_settings(value: &Value, req: &NativeRequest) -> Result<()> {
    ensure!(
        value.pointer("/applied/model").and_then(Value::as_str) == Some(&req.model),
        "Claude applied model differs from the explicit requested model"
    );
    if let Some(effort) = &req.reasoning_effort {
        ensure!(
            value.pointer("/applied/effort").and_then(Value::as_str) == Some(effort),
            "Claude cannot apply requested reasoning effort"
        );
    }
    let effective = value["effective"]
        .as_object()
        .context("Claude effective settings unavailable")?;
    let expected = settings();
    // Fail closed for unknown managed settings. In safe mode policy remains
    // authoritative; the host refuses incompatible policy, never bypasses it.
    ensure!(
        Value::Object(effective.clone()) == expected,
        "Claude effective settings differ from the restricted host configuration"
    );
    let sources = value["sources"]
        .as_array()
        .context("Claude settings source evidence missing")?;
    for source in sources {
        let name = source["source"]
            .as_str()
            .context("Claude settings source name missing")?;
        if name != "flagSettings" {
            ensure!(
                source["settings"].as_object().is_some_and(|v| v.is_empty()),
                "Claude inherited external or managed settings; use the manual terminal"
            );
        }
    }
    Ok(())
}

fn normalize_questions(input: &Value) -> Result<Value> {
    let questions = input["questions"]
        .as_array()
        .context("Claude questions missing")?;
    ensure!(
        !questions.is_empty() && questions.len() <= 4,
        "invalid Claude question count"
    );
    let mut seen = HashSet::new();
    let mut normalized = Vec::new();
    for question in questions {
        let text = required_string(question, "question")?;
        ensure!(
            text.len() <= 8192 && seen.insert(text),
            "duplicate or oversized Claude question"
        );
        let options = question["options"]
            .as_array()
            .context("Claude question options missing")?;
        ensure!(options.len() <= 8, "too many Claude question options");
        for option in options {
            required_string(option, "label")?;
        }
        normalized.push(
            json!({"question":text, "header":question["header"], "options":options,
            "multi_select":question.get("multiSelect").and_then(Value::as_bool).unwrap_or(false)}),
        );
    }
    Ok(json!({"questions":normalized}))
}

fn validate_answers(input: &Value, answers: &Value) -> Result<()> {
    let normalized = normalize_questions(input)?;
    let questions = normalized["questions"]
        .as_array()
        .expect("normalized questions");
    let answers = answers
        .as_object()
        .context("Claude answers must map question text to strings")?;
    ensure!(
        answers.len() == questions.len(),
        "Claude answers do not match pending questions"
    );
    for question in questions {
        let key = question["question"]
            .as_str()
            .expect("normalized question text");
        let answer = answers
            .get(key)
            .and_then(Value::as_str)
            .context("Claude question answer missing")?;
        ensure!(
            !answer.trim().is_empty() && answer.len() <= 16384,
            "Claude answer empty or too large"
        );
    }
    Ok(())
}

#[cfg(test)]
include!("../../tests/claude_native/protocol.rs");
