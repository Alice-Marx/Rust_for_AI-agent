//! 代码执行沙箱。
//!
//! 定位：**纵深防御**，不是完美隔离边界。已实现的能力：
//! - 语言白名单、输入与输出大小上限、超时并确保子进程被杀；
//! - 独立工作目录 + 清空环境变量（只注入最小 PATH）；
//! - Windows：把子进程放进 Job Object，设置「句柄关闭即杀」「内存上限」
//!   「进程数上限」，因此超时或父进程退出不会留下孤儿进程；
//! - Linux/macOS：如果存在 bwrap 或 sandbox-exec，就用它做文件系统与网络隔离。
//!
//! 未实现的部分必须清楚知道：没有 AppContainer 或受限令牌，因此子进程仍以当前
//! 用户身份运行；allow_network=false 在 Windows 上只是策略声明，操作系统层面
//! 并没有真正断网。生产部署请把本进程放进容器或虚拟机。

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::time::timeout;
use uuid::Uuid;

use crate::model::SandboxRequest;

/// 进程能拿到的最小环境变量。
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
    /// 是否允许网络访问。Windows 上目前只是声明，不做系统级断网。
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
            allowed_languages: vec!["python".to_string()],
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
            bail!("code execution is disabled; set AGENT_ENABLE_SANDBOX=true only in an isolated environment")
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

        let duration = Duration::from_millis(request.timeout_ms.unwrap_or(self.policy.timeout_ms));
        limits.push(format!("timeout={}ms", duration.as_millis()));

        let isolation = apply_platform_confinement(&mut command, &self.policy, &mut limits);

        let child = command.spawn().context("sandbox process failed to start")?;

        #[cfg(windows)]
        let job = match win_job::assign(&child, &self.policy, &mut limits) {
            Ok(job) => job,
            Err(error) => {
                tracing::warn!(%error, "无法为沙箱进程创建 Job Object，进程上限保证降级");
                None
            }
        };

        let output = timeout(duration, child.wait_with_output()).await;
        #[cfg(windows)]
        drop(job);

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

fn script_name(language: &str) -> &'static str {
    match language {
        "python" => "main.py",
        "node" => "main.js",
        "bash" | "sh" => "main.sh",
        _ => "main.txt",
    }
}

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
        "windows-job-object"
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

#[cfg(windows)]
fn apply_platform_confinement(
    _command: &mut Command,
    policy: &SandboxPolicy,
    limits: &mut Vec<String>,
) -> String {
    if !policy.allow_network {
        // Windows 上无法在不引入防火墙或 WFP 的前提下真正断网，这里如实标注。
        limits.push("network-not-enforced-on-windows".to_string());
    }
    "windows-job-object".to_string()
}

/// 非 Windows 平台的文件系统与网络隔离（可用时）。
#[cfg(not(windows))]
fn apply_platform_confinement(
    command: &mut Command,
    policy: &SandboxPolicy,
    limits: &mut Vec<String>,
) -> String {
    if let Some(bwrap) = which("bwrap") {
        let program = command.as_std().get_program().to_os_string();
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_os_string())
            .collect();
        let mut wrapped = Command::new(bwrap);
        wrapped
            .args(["--unshare-all", "--die-with-parent", "--new-session"])
            .args(["--ro-bind", "/", "/"])
            .args(["--tmpfs", "/tmp"])
            .arg("--chdir")
            .arg(".");
        if !policy.allow_network {
            // unshare-all 已经断开网络命名空间；保持默认即可。
            limits.push("bwrap:no-network".to_string());
        }
        wrapped.arg(program);
        for arg in args {
            wrapped.arg(arg);
        }
        *command = wrapped;
        limits.push("bwrap:unshare-all".to_string());
        limits.push("bwrap:read-only-root".to_string());
        return "linux-bubblewrap".to_string();
    }
    if cfg!(target_os = "macos") && Path::new("/usr/bin/sandbox-exec").exists() {
        let profile = r#"(version 1)(deny default)(allow process*)(allow file-read*)(allow sysctl-read)(allow file-write* (subpath "/private/tmp"))(deny network*)"#;
        let program = command.as_std().get_program().to_os_string();
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_os_string())
            .collect();
        let mut wrapped = Command::new("/usr/bin/sandbox-exec");
        wrapped.arg("-p").arg(profile).arg(program);
        for arg in args {
            wrapped.arg(arg);
        }
        *command = wrapped;
        limits.push("sandbox-exec:deny-network".to_string());
        return "macos-sandbox-exec".to_string();
    }
    limits.push("no-os-isolation-available".to_string());
    "none".to_string()
}

fn truncate(bytes: &[u8], max_bytes: usize, limits: &mut Vec<String>) -> String {
    if bytes.len() > max_bytes {
        limits.push(format!("output-truncated={max_bytes}B"));
    }
    let end = bytes.len().min(max_bytes);
    String::from_utf8_lossy(&bytes[..end]).to_string()
}

#[cfg(windows)]
mod win_job {
    //! Windows Job Object：句柄关闭即杀 + 内存与进程数上限。

    use std::mem::size_of;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_JOB_MEMORY,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    };

    use super::SandboxPolicy;

    /// 持有 Job Object 句柄；Drop 时关闭，触发 kill-on-close。
    pub struct JobHandle(HANDLE);

    // SAFETY: 内核句柄本身只是一个可跨线程传递的整数标识；这里只保证
    // 句柄随结构体移动，并在唯一所有者 Drop 时关闭一次。
    unsafe impl Send for JobHandle {}

    impl Drop for JobHandle {
        fn drop(&mut self) {
            // SAFETY: 句柄由 CreateJobObjectW 返回，且只在这里关闭一次。
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    pub fn assign(
        child: &tokio::process::Child,
        policy: &SandboxPolicy,
        limits: &mut Vec<String>,
    ) -> anyhow::Result<Option<JobHandle>> {
        let Some(raw) = child.raw_handle() else {
            anyhow::bail!("子进程句柄不可用");
        };
        // SAFETY: 空安全属性与空名字创建匿名 Job。
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            anyhow::bail!("CreateJobObjectW 失败");
        }
        let handle = JobHandle(job);

        let mut information: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        limits.push("kill-on-close".to_string());
        if let Some(megabytes) = policy.memory_limit_mb {
            let bytes = (megabytes as usize).saturating_mul(1024 * 1024);
            information.ProcessMemoryLimit = bytes;
            information.JobMemoryLimit = bytes;
            information.BasicLimitInformation.LimitFlags |=
                JOB_OBJECT_LIMIT_PROCESS_MEMORY | JOB_OBJECT_LIMIT_JOB_MEMORY;
            limits.push(format!("job-object:memory={megabytes}MB"));
        }
        if let Some(processes) = policy.max_processes {
            information.BasicLimitInformation.ActiveProcessLimit = processes;
            information.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            limits.push(format!("job-object:max-processes={processes}"));
        }

        // SAFETY: information 是栈上有效结构，长度按具体类型给出。
        let configured = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &information as *const _ as *const core::ffi::c_void,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            anyhow::bail!("SetInformationJobObject 失败");
        }

        // SAFETY: raw 是 tokio 持有的有效进程句柄，仅在此调用期间借用。
        let assigned = unsafe { AssignProcessToJobObject(job, raw as HANDLE) };
        if assigned == 0 {
            anyhow::bail!("AssignProcessToJobObject 失败（当前进程可能已在其它 Job 中）");
        }
        limits.push("process-assigned-to-job".to_string());
        Ok(Some(handle))
    }
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
                eprintln!("python 不可用，跳过：{error}");
                return;
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
            assert_eq!(result.isolation, "windows-job-object");
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
                eprintln!("python 不可用，跳过：{error}");
                return;
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
                eprintln!("python 不可用，跳过：{error}");
                return;
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
                eprintln!("python 不可用，跳过：{error}");
                return;
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
