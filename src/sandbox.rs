use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::{fs, process::Command, time::timeout};
use uuid::Uuid;

use crate::model::SandboxRequest;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxPolicy {
    pub enabled: bool,
    pub timeout_ms: u64,
    pub max_output_bytes: usize,
    pub allowed_languages: Vec<String>,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout_ms: 2_000,
            max_output_bytes: 64 * 1024,
            allowed_languages: vec!["python".to_string()],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
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

    /// Run code with an explicit allow-list and time/output limits.
    ///
    /// This is intentionally a development sandbox, not a security boundary:
    /// a production deployment must put the process in a container or VM with
    /// a separate user, seccomp/AppContainer policy, no network, and quotas.
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

        let directory =
            std::env::temp_dir().join(format!("rust-ai-agent-sandbox-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).await?;
        let code_path = directory.join("main.py");
        fs::write(&code_path, request.code).await?;

        let mut command = Command::new("python");
        command
            .args(["-I", "-S"])
            .arg(&code_path)
            .current_dir(&directory)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .kill_on_drop(true);

        let duration = Duration::from_millis(request.timeout_ms.unwrap_or(self.policy.timeout_ms));
        let result = match timeout(duration, command.output()).await {
            Ok(output) => {
                let output = output.context("sandbox process failed to start")?;
                SandboxResult {
                    stdout: truncate(output.stdout, self.policy.max_output_bytes),
                    stderr: truncate(output.stderr, self.policy.max_output_bytes),
                    exit_code: output.status.code(),
                    timed_out: false,
                }
            }
            Err(_) => SandboxResult {
                stdout: String::new(),
                stderr: "sandbox timed out".to_string(),
                exit_code: None,
                timed_out: true,
            },
        };

        let _ = fs::remove_dir_all(&directory).await;
        Ok(result)
    }
}

fn truncate(bytes: Vec<u8>, max_bytes: usize) -> String {
    let end = bytes.len().min(max_bytes);
    String::from_utf8_lossy(&bytes[..end]).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sandbox_is_disabled_by_default() {
        let sandbox = SandboxExecutor::new(SandboxPolicy::default());
        let error = sandbox
            .execute(SandboxRequest {
                language: "python".to_string(),
                code: "print(1)".to_string(),
                timeout_ms: None,
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("disabled"));
    }
}
