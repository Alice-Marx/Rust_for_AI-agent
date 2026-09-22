//! Exact-release ACP adapter for the unmodified official DeepSeek Harness CLI.
//! ACP v0.1.6 emits committed message blocks, not provider token deltas.
use super::acp::{AcpDialect, AcpRunner};
use super::{
    cancellation, emit, empty_result, executable_digest, read_frame, NativeControl, NativeEvent,
    NativeRequest, NativeResult, MAX_FRAME_BYTES, MAX_OUTPUT_BYTES,
};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::BufReader,
    sync::{mpsc, watch},
};

const VERSION: &str = "0.1.6-alpha.2";
const PROVIDER: &str = "deepseek-official";
const CLI_HASH: &str = "69c49c871735dc7ee81ec51f266bbec129f075fd5066e046374f4b13ab02a705";
const BASE_HASH: &str = "a395d4e6b1b4de3694d354ab21901c174f59a30557d906f6324a474f486b2cc8";
const ACP_HASH: &str = "3c559e8860348f1a878637a740bb11f80863f8e97b895060badda962a8a127fd";

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
    let mut runner = AcpRunner::new(
        Box::new(DeepSeekDialect),
        Box::new(stdin),
        rx,
        controls,
        events.clone(),
        &req,
    );
    let outcome = tokio::select! {
        result = runner.run(&req) => result,
        _ = cancellation(&mut cancel) => Ok("cancelled".into()),
        _ = tokio::time::sleep_until(deadline) => Ok("timed_out".into()),
        _ = events.closed() => Err(anyhow::anyhow!("DeepSeek event consumer disconnected")),
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

/// DeepSeek Harness ACP dialect: strict identity, JSON-array model values,
/// one-shot permissions with exactly two options.
pub(super) struct DeepSeekDialect;

impl AcpDialect for DeepSeekDialect {
    fn label(&self) -> &'static str {
        "DeepSeek"
    }
    fn protocol(&self) -> &'static str {
        "deepseek-acp"
    }
    fn request_prefix(&self) -> &'static str {
        "deepseek"
    }
    fn status_note(&self) -> String {
        "DeepSeek official ACP: committed message blocks; usage reports context occupancy, not billed tokens. The official file sandbox applies; Windows confinement has upstream documented limitations.".into()
    }
    fn verify_initialize(&self, init: &Value) -> Result<()> {
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
        Ok(())
    }
    fn model_config_id(&self) -> &'static str {
        "model"
    }
    fn model_value(&self, req: &NativeRequest) -> String {
        json!([PROVIDER, req.model]).to_string()
    }
    fn effort_config_id(&self) -> Option<&'static str> {
        Some("reasoning_effort")
    }
    fn verify_config_options(
        &self,
        options: &Value,
        model: &str,
        effort: Option<&str>,
    ) -> Result<()> {
        let options = options
            .as_array()
            .context("DeepSeek ACP config options missing")?;
        let models: Vec<_> = options
            .iter()
            .filter(|item| item["id"] == "model")
            .collect();
        ensure!(
            models.len() == 1
                && models[0]["currentValue"]
                    .as_str()
                    .is_some_and(|value| value == &json!([PROVIDER, model]).to_string()),
            "DeepSeek provider/model drift detected"
        );
        if let Some(effort) = effort {
            let efforts: Vec<_> = options
                .iter()
                .filter(|item| item["id"] == "reasoning_effort")
                .collect();
            ensure!(
                efforts.len() == 1 && efforts[0]["currentValue"].as_str() == Some(effort),
                "DeepSeek reasoning effort drift detected"
            );
        }
        Ok(())
    }
    fn permission_selection(&self, options: &[Value]) -> Result<(String, String)> {
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
        Ok(("allow-once".into(), "reject-once".into()))
    }
    fn passthrough_updates(&self) -> &'static [&'static str] {
        &[]
    }
    fn stop_status(&self, reason: Option<&str>) -> Result<String> {
        match reason {
            Some("end_turn") => Ok("completed".into()),
            Some("cancelled") => Ok("cancelled".into()),
            Some("max_tokens" | "max_turn_requests") => {
                bail!("DeepSeek stopped at an execution limit; task remains incomplete")
            }
            _ => bail!("DeepSeek ACP returned an unsupported stop reason"),
        }
    }
}

#[cfg(test)]

mod tests {
    use super::*;

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
}

mod recorded {
    use super::super::read_frame;
    use anyhow::Result;
    use serde_json::Value;
    use std::{
        pin::Pin,
        sync::{Arc, Mutex},
        task::{Context as TaskContext, Poll},
    };
    use tokio::io::AsyncWrite;

    #[derive(Clone, Default)]
    pub(super) struct Recorded(pub Arc<Mutex<Vec<u8>>>);
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
    pub(super) async fn read_json_frames(
        reader: &mut tokio::io::BufReader<impl tokio::io::AsyncRead + Unpin>,
    ) -> Result<Vec<Value>> {
        let mut frames = Vec::new();
        let mut frame = Vec::new();
        while read_frame(reader, &mut frame).await? {
            frames.push(serde_json::from_slice(&frame)?);
            frame.clear();
        }
        Ok(frames)
    }
    pub(super) const SESSION: &str = "bec6941c-a1ce-4cfe-bb28-c1c396f9f1e5";
}

#[cfg(test)]
mod live_tests {
    use super::recorded::{read_json_frames, Recorded, SESSION};
    use super::*;
    use anyhow::Result;
    use serde_json::{json, Value};
    use std::time::Duration;
    use tokio::{io::BufReader, sync::mpsc};

    fn request() -> NativeRequest {
        NativeRequest {
            app_id: "deepseek".into(),
            model: "deepseek-v4-pro".into(),
            cwd: std::env::current_dir().unwrap(),
            prompt: "Reply with the single word ok and stop.".into(),
            read_only: true,
            max_duration_secs: 120,
            reasoning_effort: None,
            config_path: None,
        }
    }

    #[tokio::test]
    #[ignore = "requires WONDERLAND_DSH_CLI and the official exact npm release"]
    async fn official_keyless_initialize_configure_close() {
        let req = request();
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
        let mut runner = AcpRunner::new(
            Box::new(DeepSeekDialect),
            Box::new(stdin),
            rx,
            controls,
            events,
            &req,
        );
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
