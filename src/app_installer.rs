//! One-click installation for registered official CLI tools.
//!
//! The install command for each application is fixed in the registry below.
//! Clients only name an application ID; no executable path, package name, or
//! argument from the request ever reaches the spawned process. Versions follow
//! the pins documented by the managed adapters so a one-click install produces
//! the exact build the executor dialects were checked against.

use crate::desktop_bridge::{self, LaunchSpec};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};
use tokio::{process::Command, task::JoinHandle};

/// How long a single install may run before it is killed.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(900);
/// Bounded tail of merged stdout/stderr lines kept for display.
const OUTPUT_LINE_CAP: usize = 400;
/// Only one installer runs at a time; npm global installs must not interleave.
static INSTALL_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// A fixed install recipe. `Unsupported` always carries an actionable reason.
pub enum InstallRecipe {
    /// `npm install -g <package>[@<version>]`
    NpmGlobal {
        package: &'static str,
        version: Option<&'static str>,
    },
    /// `uv tool install <package>` (Python tooling; requires uv on PATH).
    UvTool { package: &'static str },
    Unsupported { reason: &'static str },
}

impl InstallRecipe {
    pub fn is_supported(&self) -> bool {
        !matches!(self, InstallRecipe::Unsupported { .. })
    }
    pub fn label(&self) -> String {
        match self {
            InstallRecipe::NpmGlobal { package, version } => match version {
                Some(version) => format!("npm install -g {package}@{version}"),
                None => format!("npm install -g {package}"),
            },
            InstallRecipe::UvTool { package } => format!("uv tool install {package}"),
            InstallRecipe::Unsupported { .. } => "无一键安装渠道".into(),
        }
    }
}

/// The registry is the single source of truth. It intentionally covers every
/// registered application so the UI can render a deterministic action for all.
pub fn install_recipe(app_id: &str) -> InstallRecipe {
    match app_id {
        // Versions pinned to the adapter-verified builds.
        "kimi-code" => InstallRecipe::NpmGlobal {
            package: "@moonshot-ai/kimi-code",
            version: Some("2.0.2"),
        },
        "deepseek" => InstallRecipe::NpmGlobal {
            package: "@deepseek-ai/dsh",
            version: Some("0.1.6-alpha.2"),
        },
        "claude" => InstallRecipe::NpmGlobal {
            package: "@anthropic-ai/claude-code",
            version: Some("2.1.193"),
        },
        "minimax" => InstallRecipe::NpmGlobal {
            package: "minimax-code",
            version: Some("0.5.2"),
        },
        "mimo" => InstallRecipe::NpmGlobal {
            package: "@mimo-ai/cli",
            version: Some("0.1.15"),
        },
        // Codex banners are matched loosely; upstream latest is accepted.
        "codex" => InstallRecipe::NpmGlobal {
            package: "@openai/codex",
            version: None,
        },
        "kimi-cli" => InstallRecipe::UvTool {
            package: "kimi-cli",
        },
        // Grok Build ships through xAI's own channel, not npm/uv.
        "grok" => InstallRecipe::Unsupported {
            reason: "Grok Build 1.0.38 由 xAI 官方渠道分发，请按官方说明安装后回到本页检测",
        },
        // No verifiable public distribution exists for these.
        "zcode" => InstallRecipe::Unsupported {
            reason: "ZCode 暂无可验证的官方发行物，无法一键安装",
        },
        "wonderland" => InstallRecipe::Unsupported {
            reason: "Wonderland CLI 随主程序发布页分发，不通过工具安装器安装",
        },
        _ => InstallRecipe::Unsupported {
            reason: "应用未注册，拒绝安装未知程序",
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Running,
    Succeeded,
    Failed,
}

struct InstallJob {
    app_id: String,
    command_label: String,
    phase: Phase,
    started_at: String,
    output: Vec<String>,
    output_truncated: bool,
    exit_code: Option<i32>,
    error: Option<String>,
    probe: Option<Value>,
}

impl InstallJob {
    fn to_json(&self) -> Value {
        json!({
            "app_id": self.app_id,
            "command": self.command_label,
            "status": match self.phase {
                Phase::Running => "running",
                Phase::Succeeded => "succeeded",
                Phase::Failed => "failed",
            },
            "started_at": self.started_at,
            "output_tail": self.output,
            "output_truncated": self.output_truncated,
            "exit_code": self.exit_code,
            "error": self.error,
            "post_install_probe": self.probe,
        })
    }
}

static JOBS: OnceLock<Mutex<HashMap<String, Arc<Mutex<InstallJob>>>>> = OnceLock::new();

fn jobs() -> &'static Mutex<HashMap<String, Arc<Mutex<InstallJob>>>> {
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Whether an install is currently executing for this app.
pub fn is_running(app_id: &str) -> bool {
    jobs()
        .lock()
        .map(|map| {
            map.get(app_id)
                .and_then(|job| job.lock().ok())
                .is_some_and(|job| job.phase == Phase::Running)
        })
        .unwrap_or(false)
}

/// Latest install record for the app, if any attempt has been made.
pub fn status(app_id: &str) -> Option<Value> {
    jobs()
        .lock()
        .ok()?
        .get(app_id)
        .and_then(|job| job.lock().ok())
        .map(|job| job.to_json())
}

fn append_line(job: &mut InstallJob, line: String) {
    if job.output.len() >= OUTPUT_LINE_CAP {
        job.output.remove(0);
        job.output_truncated = true;
    }
    job.output.push(line);
}

fn resolve_tool(name: &str) -> Result<PathBuf> {
    desktop_bridge::find_executable(name)
        .with_context(|| format!("未找到 {name}。请先安装 {name} 并确保它在 PATH 中"))
}

/// One-shot at service startup: query `npm config get prefix` and append that
/// directory to this process's PATH. Users who move the npm prefix off the
/// default location (for example to save C-drive space) still get their
/// globally installed CLIs discovered by probes and executors. Best effort:
/// any failure simply leaves PATH untouched.
pub async fn ensure_npm_prefix_on_path() {
    static DONE: OnceLock<()> = OnceLock::new();
    if DONE.set(()).is_err() {
        return;
    }
    let Some(npm) = desktop_bridge::find_executable("npm") else {
        return;
    };
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let spec = match desktop_bridge::normalize_launch(
        npm,
        vec!["config".into(), "get".into(), "prefix".into()],
        home,
        "npm prefix".into(),
    ) {
        Ok(spec) => spec,
        Err(_) => return,
    };
    let mut command = Command::from(std::process::Command::new(&spec.executable));
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .envs(&spec.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let Ok(Ok(output)) = tokio::time::timeout(Duration::from_secs(5), command.output()).await
    else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    // npm may print a Windows verbatim prefix (\\?\D:\...); strip it so the
    // plain path compares equal to PATH entries.
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned();
    let prefix = PathBuf::from(&text);
    if !prefix.is_dir() {
        return;
    }
    let current = std::env::var_os("PATH").unwrap_or_default();
    let already = std::env::split_paths(&current).any(|dir| dir == prefix);
    if already {
        return;
    }
    let mut dirs: Vec<PathBuf> = std::env::split_paths(&current).collect();
    dirs.push(prefix);
    if let Ok(joined) = std::env::join_paths(dirs) {
        std::env::set_var("PATH", joined);
    }
}

/// Start an install for a registered app. Returns the initial running record.
/// Rejects unknown apps, unsupported recipes, and concurrent installs of the
/// same app. A global install mutex serializes npm operations across apps.
pub async fn start(app_id: &str) -> Result<Value> {
    ensure!(app_id.len() <= 64, "application ID is not registered");
    ensure!(
        desktop_bridge::official_apps()
            .into_iter()
            .any(|app| app.id == app_id),
        "application ID is not registered"
    );
    if let InstallRecipe::Unsupported { reason } = install_recipe(app_id) {
        bail!("该应用不支持一键安装：{reason}");
    }
    if is_running(app_id) {
        bail!("该应用已有安装任务正在执行");
    }
    // Reserve the global installer slot before building the command so a
    // missing tool reports clearly instead of a half-started job.
    let _global = INSTALL_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| anyhow::anyhow!("安装器状态不可用"))?;

    let (tool_name, args) = build_args(&install_recipe(app_id));
    let tool = resolve_tool(tool_name)?;
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let label = install_recipe(app_id).label();
    let spec: LaunchSpec =
        desktop_bridge::normalize_launch(tool, args, home, format!("install {app_id}"))?;

    let job = Arc::new(Mutex::new(InstallJob {
        app_id: app_id.to_owned(),
        command_label: label,
        phase: Phase::Running,
        started_at: chrono::Utc::now().to_rfc3339(),
        output: Vec::new(),
        output_truncated: false,
        exit_code: None,
        error: None,
        probe: None,
    }));
    jobs()
        .lock()
        .map_err(|_| anyhow::anyhow!("安装器状态不可用"))?
        .insert(app_id.to_owned(), job.clone());

    let runner: JoinHandle<()> =
        tokio::spawn(run_install(spec, app_id.to_owned(), job.clone()));
    // Detached bookkeeping: keep the join handle alive without awaiting here.
    tokio::spawn(async move {
        let _ = runner.await;
    });
    let initial = job
        .lock()
        .map_err(|_| anyhow::anyhow!("安装器状态不可用"))?
        .to_json();
    Ok(initial)
}

fn build_args(recipe: &InstallRecipe) -> (&'static str, Vec<String>) {
    match recipe {
        InstallRecipe::NpmGlobal { package, version } => {
            let mut args = vec!["install".to_owned(), "-g".to_owned()];
            match version {
                Some(version) => args.push(format!("{package}@{version}")),
                None => args.push((*package).to_owned()),
            }
            ("npm", args)
        }
        InstallRecipe::UvTool { package } => (
            "uv",
            vec![
                "tool".to_owned(),
                "install".to_owned(),
                (*package).to_owned(),
            ],
        ),
        InstallRecipe::Unsupported { .. } => unreachable!("rejected before build_args"),
    }
}

async fn run_install(spec: LaunchSpec, app_id: String, job: Arc<Mutex<InstallJob>>) {
    let started = Instant::now();
    let mut command = Command::from(std::process::Command::new(&spec.executable));
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .envs(&spec.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command.kill_on_drop(true);
    let mut child = match command
        .spawn()
        .with_context(|| format!("无法启动安装命令：{}", spec.label))
    {
        Ok(child) => child,
        Err(error) => {
            finish(&app_id, &job, Phase::Failed, None, Some(error.to_string())).await;
            return;
        }
    };
    // Stream both pipes into the bounded output tail; npm writes progress to
    // stderr and registry chatter to stdout.
    let readers = [
        child
            .stdout
            .take()
            .map(|stream| pump::spawn_with_job(stream, job.clone())),
        child
            .stderr
            .take()
            .map(|stream| pump::spawn_with_job(stream, job.clone())),
    ];
    let outcome = tokio::time::timeout(INSTALL_TIMEOUT, child.wait()).await;
    match outcome {
        Ok(Ok(status)) if status.success() => {
            finish(&app_id, &job, Phase::Succeeded, status.code(), None).await
        }
        Ok(Ok(status)) => {
            let code = status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "信号终止".into());
            finish(
                &app_id,
                &job,
                Phase::Failed,
                status.code(),
                Some(format!(
                    "安装命令退出码非零（{code}），耗时 {:.0}s",
                    started.elapsed().as_secs_f64()
                )),
            )
            .await
        }
        Ok(Err(error)) => finish(&app_id, &job, Phase::Failed, None, Some(error.to_string())).await,
        Err(_) => {
            // Timeout: dropping the child triggers kill_on_drop.
            drop(child);
            finish(
                &app_id,
                &job,
                Phase::Failed,
                None,
                Some(format!("安装超时（{} 秒）已终止", INSTALL_TIMEOUT.as_secs())),
            )
            .await
        }
    }
    for reader in readers.into_iter().flatten() {
        reader.abort();
    }
}

/// Reads one piped stream line by line into the shared bounded tail.
mod pump {
    use super::{append_line, InstallJob};
    use std::sync::{Arc, Mutex};
    use tokio::{io::AsyncBufReadExt, task::JoinHandle};

    pub(super) fn spawn_with_job<R>(reader: R, job: Arc<Mutex<InstallJob>>) -> JoinHandle<()>
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(mut job) = job.lock() {
                    append_line(&mut job, line);
                }
            }
        })
    }
}

async fn finish(
    app_id: &str,
    job: &Arc<Mutex<InstallJob>>,
    phase: Phase,
    exit_code: Option<i32>,
    error: Option<String>,
) {
    // A successful install is confirmed the same way the apps page does it:
    // the read-only version probe, never the installer's own claim.
    let probe = if phase == Phase::Succeeded {
        crate::app_diagnostics::probe(app_id).await.ok()
    } else {
        None
    };
    if let Ok(mut job) = job.lock() {
        job.phase = phase;
        job.exit_code = exit_code;
        job.error = error;
        job.probe = probe;
        append_line(
            &mut job,
            format!(
                "— 安装{} —",
                if phase == Phase::Succeeded {
                    "完成"
                } else {
                    "失败"
                }
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registered_ids() -> Vec<String> {
        desktop_bridge::official_apps()
            .into_iter()
            .map(|app| app.id)
            .collect()
    }

    #[test]
    fn every_registered_app_has_a_recipe() {
        for id in registered_ids() {
            if let InstallRecipe::Unsupported { reason } = install_recipe(&id) {
                assert!(!reason.is_empty(), "{id} needs an actionable reason");
            }
        }
    }

    #[test]
    fn npm_recipes_pin_adapter_verified_versions() {
        for (id, package, version) in [
            ("kimi-code", "@moonshot-ai/kimi-code", Some("2.0.2")),
            ("deepseek", "@deepseek-ai/dsh", Some("0.1.6-alpha.2")),
            ("claude", "@anthropic-ai/claude-code", Some("2.1.193")),
            ("minimax", "minimax-code", Some("0.5.2")),
            ("mimo", "@mimo-ai/cli", Some("0.1.15")),
            ("codex", "@openai/codex", None),
        ] {
            match install_recipe(id) {
                InstallRecipe::NpmGlobal {
                    package: actual,
                    version: actual_version,
                } => {
                    assert_eq!(actual, package, "{id} package mismatch");
                    assert_eq!(actual_version, version.as_deref(), "{id} version mismatch");
                }
                other => panic!("{id} expected NpmGlobal, got {}", other.label()),
            }
        }
    }

    #[test]
    fn unsupported_recipes_are_fail_closed() {
        for id in ["grok", "zcode", "wonderland", "not-an-app", ""] {
            assert!(
                matches!(install_recipe(id), InstallRecipe::Unsupported { .. }),
                "{id} must not be installable"
            );
        }
    }

    #[test]
    fn unknown_app_ids_are_rejected_by_start() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            for id in ["not-an-app", "", "grok"] {
                assert!(start(id).await.is_err(), "{id} must be rejected");
            }
        });
    }

    #[test]
    fn output_tail_is_bounded() {
        let mut job = InstallJob {
            app_id: "test".into(),
            command_label: "test".into(),
            phase: Phase::Running,
            started_at: String::new(),
            output: Vec::new(),
            output_truncated: false,
            exit_code: None,
            error: None,
            probe: None,
        };
        for i in 0..(OUTPUT_LINE_CAP + 50) {
            append_line(&mut job, format!("line {i}"));
        }
        assert_eq!(job.output.len(), OUTPUT_LINE_CAP);
        assert!(job.output_truncated);
        assert_eq!(
            job.output.last().unwrap(),
            &format!("line {}", OUTPUT_LINE_CAP + 49)
        );
    }
}
