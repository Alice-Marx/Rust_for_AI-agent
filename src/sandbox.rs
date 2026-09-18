//! OS-enforced execution: Windows AppContainer, Linux bubblewrap, macOS seatbelt.
//! Missing isolation fails closed. Only disposable run directories are writable.

use std::path::{Path, PathBuf};
#[cfg(not(windows))]
use std::time::Duration;

#[cfg(not(windows))]
use anyhow::Context;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
#[cfg(not(windows))]
use tokio::process::Command;
#[cfg(not(windows))]
use tokio::time::timeout;
use uuid::Uuid;

use crate::model::SandboxRequest;

/// 进程能拿到的最小环境变量。
#[cfg(not(windows))]
const MINIMAL_ENV: [(&str, &str); 2] = [
    ("PYTHONIOENCODING", "utf-8"),
    ("PYTHONDONTWRITEBYTECODE", "1"),
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxPolicy {
    pub enabled: bool,
    pub timeout_ms: u64,
    pub max_output_bytes: usize,
    pub allowed_languages: Vec<String>,
    /// 单进程内存上限（Windows Job Object 生效；其它平台仅记录）。
    #[serde(default)]
    pub memory_limit_mb: Option<u64>,
    /// 同一 Job 内的最大进程数（防止 fork 炸弹）。
    #[serde(default)]
    pub max_processes: Option<u32>,
    /// 是否允许网络访问。Windows AppContainer 固定禁网，开启会返回错误。
    #[serde(default)]
    pub allow_network: bool,
    /// 工作目录根；缺省使用系统临时目录下的独立子目录。
    #[serde(default)]
    pub working_root: Option<PathBuf>,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout_ms: 2_000,
            max_output_bytes: 64 * 1024,
            allowed_languages: vec!["python".to_string(), "node".to_string()],
            memory_limit_mb: None,
            max_processes: None,
            allow_network: false,
            working_root: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// 本次真正生效的约束清单（没生效的不会写进来）。
    #[serde(default)]
    pub limits: Vec<String>,
    /// 实际使用的隔离机制。
    #[serde(default)]
    pub isolation: String,
}

#[derive(Clone)]
pub struct SandboxExecutor {
    policy: SandboxPolicy,
}

impl SandboxExecutor {
    pub fn new(policy: SandboxPolicy) -> Self {
        Self { policy }
    }

    pub fn policy(&self) -> &SandboxPolicy {
        &self.policy
    }

    /// 供 /health 展示的运行时沙箱画像。
    pub fn describe(&self) -> serde_json::Value {
        serde_json::json!({
            "enabled": self.policy.enabled,
            "timeout_ms": self.policy.timeout_ms,
            "max_output_bytes": self.policy.max_output_bytes,
            "allowed_languages": self.policy.allowed_languages,
            "memory_limit_mb": self.policy.memory_limit_mb,
            "max_processes": self.policy.max_processes,
            "allow_network": self.policy.allow_network,
            "isolation": platform_isolation(),
        })
    }

    pub async fn execute(&self, request: SandboxRequest) -> Result<SandboxResult> {
        if !self.policy.enabled {
            bail!("code execution is disabled; set AGENT_ENABLE_SANDBOX=true to enable OS-isolated execution")
        }
        let language = request.language.trim().to_lowercase();
        if !self
            .policy
            .allowed_languages
            .iter()
            .any(|allowed| allowed == &language)
        {
            bail!("language '{language}' is not allowed by the sandbox policy")
        }
        if request.code.len() > self.policy.max_output_bytes {
            bail!("code exceeds the sandbox input limit")
        }

        let directory = match &self.policy.working_root {
            Some(root) => {
                tokio::fs::create_dir_all(root).await?;
                root.join(format!("run-{}", Uuid::new_v4()))
            }
            None => std::env::temp_dir().join(format!("wonderland-sandbox-{}", Uuid::new_v4())),
        };
        tokio::fs::create_dir_all(&directory).await?;
        let script = directory.join(script_name(&language));
        tokio::fs::write(&script, request.code.as_bytes()).await?;

        #[cfg(windows)]
        {
            let run_dir = directory.clone();
            let policy = self.policy.clone();
            let timeout_ms = request
                .timeout_ms
                .unwrap_or(policy.timeout_ms)
                .min(policy.timeout_ms)
                .max(1);
            let result = tokio::task::spawn_blocking(move || {
                crate::sandbox_windows::execute(&language, &script, &run_dir, &policy, timeout_ms)
            })
            .await?;
            let _ = tokio::fs::remove_dir_all(&directory).await;
            return result;
        }

        #[cfg(not(windows))]
        {
            let mut limits: Vec<String> = Vec::new();
            if self.policy.allow_network {
                limits.push("network-allowed-by-policy".to_string());
            } else {
                limits.push("network-denied-by-policy".to_string());
            }
            let mut command = build_command(&language, &script, &mut limits);
            command.current_dir(&directory);
            command.env_clear();
            command.env("PATH", std::env::var("PATH").unwrap_or_default());
            for (name, value) in MINIMAL_ENV {
                command.env(name, value);
            }
            command.kill_on_drop(true);
            // 不设 piped 时 wait_with_output 拿不到任何输出（会继承父进程的 stdio）。
            command.stdout(std::process::Stdio::piped());
            command.stderr(std::process::Stdio::piped());
            command.stdin(std::process::Stdio::null());
            limits.push(format!("cwd-isolated={}", directory.display()));
            limits.push("env-cleared".to_string());

            let duration = Duration::from_millis(
                request
                    .timeout_ms
                    .unwrap_or(self.policy.timeout_ms)
                    .min(self.policy.timeout_ms)
                    .max(1),
            );
            limits.push(format!("timeout={}ms", duration.as_millis()));

            let isolation = apply_platform_confinement(&mut command, &self.policy, &mut limits)?;

            let child = command.spawn().context("sandbox process failed to start")?;

            let output = timeout(
                duration,
                bounded_output(child, self.policy.max_output_bytes),
            )
            .await;

            let result = match output {
                Ok(Ok(output)) => SandboxResult {
                    stdout: truncate(&output.stdout, self.policy.max_output_bytes, &mut limits),
                    stderr: truncate(&output.stderr, self.policy.max_output_bytes, &mut limits),
                    exit_code: output.status.code(),
                    timed_out: false,
                    limits,
                    isolation,
                },
                Ok(Err(error)) => {
                    let _ = tokio::fs::remove_dir_all(&directory).await;
                    return Err(error).context("sandbox process failed");
                }
                Err(_) => SandboxResult {
                    stdout: String::new(),
                    // kill_on_drop 与 Job Object 的 kill-on-close 共同保证进程被杀。
                    stderr: format!("sandbox timed out after {} ms", duration.as_millis()),
                    exit_code: None,
                    timed_out: true,
                    limits,
                    isolation,
                },
            };

            let _ = tokio::fs::remove_dir_all(&directory).await;
            Ok(result)
        }
    }
}

fn script_name(language: &str) -> &'static str {
    match language {
        "python" => "main.py",
        "node" => "main.js",
        "bash" | "sh" => "main.sh",
        _ => "main.txt",
    }
}

#[cfg(not(windows))]
fn build_command(language: &str, script: &Path, limits: &mut Vec<String>) -> Command {
    match language {
        "python" => {
            let mut command = Command::new("python");
            // -I 隔离模式：忽略环境变量与用户 site-packages，减少宿主注入面。
            command.arg("-I").arg("-S").arg(script);
            limits.push("python-isolated-mode".to_string());
            command
        }
        "node" => {
            let mut command = Command::new("node");
            command.arg(script);
            limits.push("node-script-mode".to_string());
            command
        }
        other => {
            let mut command = Command::new(other);
            command.arg(script);
            limits.push(format!("interpreter={other}"));
            command
        }
    }
}

fn platform_isolation() -> &'static str {
    if cfg!(windows) {
        "windows-appcontainer"
    } else if which("bwrap").is_some() {
        "linux-bubblewrap"
    } else if cfg!(target_os = "macos") && Path::new("/usr/bin/sandbox-exec").exists() {
        "macos-sandbox-exec"
    } else {
        "none"
    }
}

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
}

#[cfg(not(windows))]
fn apply_platform_confinement(
    command: &mut Command,
    policy: &SandboxPolicy,
    limits: &mut Vec<String>,
) -> Result<String> {
    let original = command.as_std();
    let program = which(&original.get_program().to_string_lossy())
        .context("sandbox interpreter unavailable")?;
    let args: Vec<_> = original.get_args().map(|s| s.to_os_string()).collect();
    let cwd = original
        .get_current_dir()
        .context("sandbox cwd missing")?
        .canonicalize()?;
    let env: Vec<_> = original
        .get_envs()
        .filter_map(|(k, v)| v.map(|v| (k.to_os_string(), v.to_os_string())))
        .collect();
    let (mut wrapped, isolation) = if let Some(bwrap) = which("bwrap") {
        let mut wrapped = Command::new(bwrap);
        wrapped.args([
            "--unshare-all",
            "--die-with-parent",
            "--new-session",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--tmpfs",
            "/tmp",
        ]);
        for root in [
            "/usr",
            "/bin",
            "/lib",
            "/lib64",
            "/etc/ld.so.cache",
            "/etc/alternatives",
        ] {
            if Path::new(root).exists() {
                wrapped.args(["--ro-bind", root, root]);
            }
        }
        wrapped
            .arg("--bind")
            .arg(&cwd)
            .arg(&cwd)
            .arg("--chdir")
            .arg(&cwd);
        if policy.allow_network {
            wrapped.arg("--share-net");
        } else {
            limits.push("bwrap:no-network".into());
        }
        wrapped.arg(&program).args(&args);
        (wrapped, "linux-bubblewrap")
    } else if cfg!(target_os = "macos") && Path::new("/usr/bin/sandbox-exec").is_file() {
        let escaped = cwd
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        let profile = format!("(version 1)(deny default)(allow process*)(allow sysctl-read)(allow mach-lookup)(allow file-read* (subpath \"/System\") (subpath \"/usr\") (subpath \"/Library\") (subpath \"/opt/homebrew\") (literal \"/dev/null\") (subpath \"{escaped}\"))(allow file-write* (subpath \"{escaped}\")){}",if policy.allow_network {"(allow network*)"} else {"(deny network*)"});
        let mut wrapped = Command::new("/usr/bin/sandbox-exec");
        wrapped.arg("-p").arg(profile).arg(&program).args(&args);
        (wrapped, "macos-sandbox-exec")
    } else {
        bail!("OS sandbox unavailable; install bubblewrap on Linux. Execution refused");
    };
    wrapped
        .current_dir(&cwd)
        .env_clear()
        .envs(env)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    *command = wrapped;
    limits.push("filesystem-restricted".into());
    Ok(isolation.into())
}

#[cfg(not(windows))]
async fn bounded_output(
    mut child: tokio::process::Child,
    max: usize,
) -> std::io::Result<std::process::Output> {
    use tokio::io::AsyncReadExt;
    async fn read(
        mut reader: impl tokio::io::AsyncRead + Unpin,
        max: usize,
    ) -> std::io::Result<Vec<u8>> {
        let mut result = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let count = reader.read(&mut chunk).await?;
            if count == 0 {
                break;
            }
            let keep = count.min(max.saturating_add(1).saturating_sub(result.len()));
            result.extend_from_slice(&chunk[..keep]);
        }
        Ok(result)
    }
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (status, stdout, stderr) =
        tokio::try_join!(child.wait(), read(stdout, max), read(stderr, max))?;
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

#[cfg(not(windows))]
fn truncate(bytes: &[u8], max_bytes: usize, limits: &mut Vec<String>) -> String {
    if bytes.len() > max_bytes {
        limits.push(format!("output-truncated={max_bytes}B"));
    }
    let end = bytes.len().min(max_bytes);
    String::from_utf8_lossy(&bytes[..end]).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(language: &str, code: &str) -> SandboxRequest {
        SandboxRequest {
            language: language.to_string(),
            code: code.to_string(),
            timeout_ms: None,
        }
    }

    #[tokio::test]
    async fn sandbox_is_disabled_by_default() {
        let sandbox = SandboxExecutor::new(SandboxPolicy::default());
        let error = sandbox
            .execute(request("python", "print(1)"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("disabled"));
    }

    #[tokio::test]
    async fn rejects_unknown_language_and_oversized_code() {
        let sandbox = SandboxExecutor::new(SandboxPolicy {
            enabled: true,
            max_output_bytes: 64,
            ..SandboxPolicy::default()
        });
        let error = sandbox
            .execute(request("ruby", "puts 1"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not allowed"));

        let error = sandbox
            .execute(request("python", &"x".repeat(200)))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("input limit"));
    }

    #[tokio::test]
    async fn runs_python_and_reports_limits() {
        let sandbox = SandboxExecutor::new(SandboxPolicy {
            enabled: true,
            timeout_ms: 20_000,
            ..SandboxPolicy::default()
        });
        let result = match sandbox
            .execute(request("python", "print('sandbox-ok')"))
            .await
        {
            Ok(result) => result,
            Err(error) => {
                panic!("sandbox execution failed: {error:#}");
            }
        };
        assert!(result.stdout.contains("sandbox-ok"), "{result:?}");
        assert_eq!(result.exit_code, Some(0));
        assert!(!result.timed_out);
        assert!(result
            .limits
            .iter()
            .any(|limit| limit.starts_with("timeout=")));
        assert!(result.limits.iter().any(|limit| limit == "env-cleared"));
        assert!(result
            .limits
            .iter()
            .any(|limit| limit.starts_with("cwd-isolated=")));
        if cfg!(windows) {
            assert_eq!(result.isolation, "windows-appcontainer");
            assert!(result.limits.iter().any(|limit| limit == "kill-on-close"));
        }
    }

    #[tokio::test]
    async fn timeout_kills_the_process() {
        let sandbox = SandboxExecutor::new(SandboxPolicy {
            enabled: true,
            timeout_ms: 1_500,
            ..SandboxPolicy::default()
        });
        let request = SandboxRequest {
            language: "python".to_string(),
            code: "import time\ntime.sleep(60)".to_string(),
            timeout_ms: Some(1_000),
        };
        let result = match sandbox.execute(request).await {
            Ok(result) => result,
            Err(error) => {
                panic!("sandbox execution failed: {error:#}");
            }
        };
        assert!(result.timed_out);
        assert!(result.stderr.contains("timed out"));
        assert!(result.exit_code.is_none());
    }

    #[tokio::test]
    async fn output_is_truncated_to_the_policy_limit() {
        let sandbox = SandboxExecutor::new(SandboxPolicy {
            enabled: true,
            max_output_bytes: 1_024,
            timeout_ms: 20_000,
            ..SandboxPolicy::default()
        });
        let result = match sandbox
            .execute(request("python", "print('x' * 5000)"))
            .await
        {
            Ok(result) => result,
            Err(error) => {
                panic!("sandbox execution failed: {error:#}");
            }
        };
        // 目标代码本身允许写入 1024 字节，因此输出上限就是 1024。
        assert!(result.stdout.len() <= 1_024);
    }

    #[tokio::test]
    async fn working_directory_is_isolated_and_cleaned_up() {
        let directory = tempfile::tempdir().unwrap();
        let sandbox = SandboxExecutor::new(SandboxPolicy {
            enabled: true,
            timeout_ms: 20_000,
            working_root: Some(directory.path().to_path_buf()),
            ..SandboxPolicy::default()
        });
        let result = match sandbox
            .execute(request(
                "python",
                "import os; print(os.path.basename(os.getcwd()))",
            ))
            .await
        {
            Ok(result) => result,
            Err(error) => {
                panic!("sandbox execution failed: {error:#}");
            }
        };
        assert!(result.stdout.trim().starts_with("run-"), "{result:?}");
        // 执行结束后工作目录被清理。
        let leftovers = std::fs::read_dir(directory.path()).unwrap().count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn describe_reports_policy_and_isolation() {
        let sandbox = SandboxExecutor::new(SandboxPolicy {
            enabled: true,
            memory_limit_mb: Some(256),
            max_processes: Some(4),
            allowed_languages: vec!["python".to_string()],
            ..SandboxPolicy::default()
        });
        let described = sandbox.describe();
        assert_eq!(described["enabled"], true);
        assert_eq!(described["memory_limit_mb"], 256);
        assert_eq!(described["max_processes"], 4);
        assert_eq!(described["allowed_languages"][0], "python");
        assert!(described["isolation"].is_string());
    }
}
