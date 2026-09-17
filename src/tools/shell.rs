use std::time::Duration;

use anyhow::Result;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use super::{truncate_str, Tool, ToolContext, ToolOutput};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
/// 输出字符上限，超出部分落盘到 output_dir。
const MAX_OUTPUT_CHARS: usize = 30_000;

/// 选择 shell：AGENT_SHELL 环境变量 > Windows 上 PATH 中的 bash > cmd /C；非 Windows 用 sh -c。
pub fn default_shell() -> (String, Vec<String>) {
    if let Ok(shell) = std::env::var("AGENT_SHELL") {
        if !shell.trim().is_empty() {
            return (shell, vec!["-c".to_string()]);
        }
    }
    #[cfg(windows)]
    {
        if find_in_path("bash").is_some() {
            return ("bash".to_string(), vec!["-c".to_string()]);
        }
        ("cmd".to_string(), vec!["/C".to_string()])
    }
    #[cfg(not(windows))]
    {
        ("sh".to_string(), vec!["-c".to_string()])
    }
}

#[cfg(windows)]
fn find_in_path(name: &str) -> Option<std::path::PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        for candidate in [format!("{name}.exe"), name.to_string()] {
            let path = dir.join(candidate);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

/// 按 `&&`、`||`、`;`、`|` 拆分复合命令；单双引号内的分隔符不拆分。
/// 权限系统要求每一段都被允许才放行整条命令。
pub fn split_compound_command(command: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = command.chars().peekable();

    fn push_part(parts: &mut Vec<String>, current: &mut String) {
        let trimmed = current.trim();
        if !trimmed.is_empty() {
            parts.push(trimmed.to_string());
        }
        current.clear();
    }

    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                current.push(c);
            }
            '"' if !in_single => {
                in_double = !in_double;
                current.push(c);
            }
            '&' if !in_single && !in_double && chars.peek() == Some(&'&') => {
                chars.next();
                push_part(&mut parts, &mut current);
            }
            '|' if !in_single && !in_double => {
                if chars.peek() == Some(&'|') {
                    chars.next();
                }
                push_part(&mut parts, &mut current);
            }
            ';' if !in_single && !in_double => {
                push_part(&mut parts, &mut current);
            }
            _ => current.push(c),
        }
    }
    push_part(&mut parts, &mut current);
    parts
}

pub struct BashTool;

#[async_trait::async_trait]
impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "Bash"
    }

    fn description(&self) -> &'static str {
        "Execute a shell command and return its combined stdout/stderr. \
         Shell selection: AGENT_SHELL env var if set; on Windows, Git Bash (bash -c) when \
         available in PATH, otherwise cmd /C; on other platforms sh -c. \
         Note: on Windows with cmd /C, Unix commands (ls, grep, sleep, ...) are unavailable. \
         Output longer than 30,000 characters is truncated and the full output is saved to a file. \
         Commands time out after 120s by default (max 600s). \
         Set run_in_background=true for long-running commands (dev servers, watches): the call \
         returns a task id immediately; retrieve output with TaskOutput, stop with TaskStop."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The command to execute"
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Timeout in milliseconds (default 120000, max 600000)"
                },
                "run_in_background": {
                    "type": "boolean",
                    "description": "Start the command in the background and return a task id immediately (default false)"
                }
            },
            "required": ["command"]
        })
    }

    fn rule_contents(&self, input: &serde_json::Value) -> Vec<String> {
        input
            .get("command")
            .and_then(|v| v.as_str())
            .map(split_compound_command)
            .unwrap_or_default()
    }

    async fn call(&self, input: serde_json::Value, ctx: &mut ToolContext) -> Result<ToolOutput> {
        let Some(command) = input
            .get("command")
            .and_then(|v| v.as_str())
            .map(str::to_string)
        else {
            return Ok(ToolOutput::err("missing required parameter: command"));
        };
        let run_in_background = input
            .get("run_in_background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if run_in_background {
            match ctx.background.spawn(&command, &ctx.working_dir).await {
                Ok(task_id) => Ok(ToolOutput::ok(format!(
                    "Command started in background.\ntask_id: {task_id}\ncommand: {command}\n\n\
                     Retrieve output with TaskOutput(task_id) and stop it with TaskStop(task_id)."
                ))),
                Err(error) => Ok(ToolOutput::err(format!(
                    "failed to start background task: {error}"
                ))),
            }
        } else {
            self.run_foreground(&command, input, ctx).await
        }
    }
}

impl BashTool {
    async fn run_foreground(
        &self,
        command: &str,
        input: serde_json::Value,
        ctx: &mut ToolContext,
    ) -> Result<ToolOutput> {
        let timeout_ms = input
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .min(MAX_TIMEOUT_MS);

        let (shell, args) = default_shell();
        let mut child = match Command::new(&shell)
            .args(&args)
            .arg(command)
            .current_dir(&ctx.working_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolOutput::err(format!(
                    "failed to spawn shell '{shell}': {e}"
                )))
            }
        };

        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();
        let stdout_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(pipe) = stdout_pipe.as_mut() {
                let _ = pipe.read_to_end(&mut buf).await;
            }
            buf
        });
        let stderr_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(pipe) = stderr_pipe.as_mut() {
                let _ = pipe.read_to_end(&mut buf).await;
            }
            buf
        });

        let (timed_out, exit_code) =
            match tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait()).await {
                Ok(Ok(status)) => (false, status.code()),
                Ok(Err(e)) => {
                    return Ok(ToolOutput::err(format!("failed to wait on command: {e}")))
                }
                Err(_) => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    (true, None)
                }
            };

        let stdout = stdout_task.await.unwrap_or_default();
        let stderr = stderr_task.await.unwrap_or_default();

        let mut output = String::from_utf8_lossy(&stdout).into_owned();
        if !stderr.is_empty() {
            if !output.is_empty() && !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str(&String::from_utf8_lossy(&stderr));
        }
        if let Some(code) = exit_code {
            if code != 0 {
                output.push_str(&format!("\n(exit code: {code})"));
            }
        }
        if timed_out {
            output.push_str(&format!(
                "\n(command timed out after {timeout_ms} ms and was killed)"
            ));
        }

        let mut truncated = false;
        let mut full_output_path = None;
        if output.len() > MAX_OUTPUT_CHARS {
            truncated = true;
            let _ = tokio::fs::create_dir_all(&ctx.output_dir).await;
            let path = ctx
                .output_dir
                .join(format!("bash-{}.log", uuid::Uuid::new_v4()));
            if tokio::fs::write(&path, &output).await.is_ok() {
                let mut head = output;
                truncate_str(&mut head, MAX_OUTPUT_CHARS);
                head.push_str(&format!("\n... (full output saved to {})", path.display()));
                full_output_path = Some(path);
                output = head;
            } else {
                truncate_str(&mut output, MAX_OUTPUT_CHARS);
            }
        }

        let is_error = timed_out || exit_code.is_some_and(|c| c != 0);
        Ok(ToolOutput {
            content: output,
            is_error,
            truncated,
            full_output_path,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn make_ctx(dir: &Path) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            read_state: crate::tools::ReadFileState::new(),
            output_dir: dir.join("out"),
            session_id: "test".to_string(),
            todos: Vec::new(),
            background: crate::tools::background::BackgroundTaskRegistry::new(),
            mode: crate::permissions::PermissionMode::Default,
            pre_plan_mode: None,
        }
    }

    fn shell_is_unixy() -> bool {
        let (shell, _) = default_shell();
        shell.contains("bash") || shell.ends_with("sh")
    }

    #[test]
    fn split_simple_and_compound() {
        assert_eq!(split_compound_command("ls"), vec!["ls".to_string()]);
        assert_eq!(
            split_compound_command("git status && cargo test"),
            vec!["git status".to_string(), "cargo test".to_string()]
        );
        assert_eq!(
            split_compound_command("a || b ; c | d"),
            vec![
                "a".to_string(),
                "b".to_string(),
                "c".to_string(),
                "d".to_string()
            ]
        );
        assert!(split_compound_command("  ; && ").is_empty());
    }

    #[test]
    fn split_respects_quotes() {
        assert_eq!(
            split_compound_command("echo \"a && b\" && git status"),
            vec!["echo \"a && b\"".to_string(), "git status".to_string()]
        );
        assert_eq!(
            split_compound_command("echo 'x | y' | grep x ; echo 'p;q'"),
            vec![
                "echo 'x | y'".to_string(),
                "grep x".to_string(),
                "echo 'p;q'".to_string()
            ]
        );
    }

    #[test]
    fn rule_contents_splits_command() {
        let contents = BashTool.rule_contents(&serde_json::json!({
            "command": "git status && cargo build"
        }));
        assert_eq!(
            contents,
            vec!["git status".to_string(), "cargo build".to_string()]
        );
        assert!(BashTool.rule_contents(&serde_json::json!({})).is_empty());
    }

    #[tokio::test]
    async fn bash_echo() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = make_ctx(dir.path());
        let out = BashTool
            .call(serde_json::json!({"command": "echo hello-agent"}), &mut ctx)
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("hello-agent"), "{}", out.content);
    }

    #[tokio::test]
    async fn bash_nonzero_exit_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = make_ctx(dir.path());
        // cmd /C 与 Unix shell 对 "exit 3" 的行为一致：退出码 3。
        let out = BashTool
            .call(serde_json::json!({"command": "exit 3"}), &mut ctx)
            .await
            .unwrap();
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("exit code: 3"), "{}", out.content);
    }

    #[tokio::test]
    async fn bash_timeout_kills_command() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = make_ctx(dir.path());
        let command = if shell_is_unixy() {
            "sleep 30"
        } else {
            "ping -n 30 127.0.0.1 >nul"
        };
        let out = BashTool
            .call(
                serde_json::json!({"command": command, "timeout_ms": 1000}),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("timed out"), "{}", out.content);
    }

    #[tokio::test]
    async fn bash_large_output_is_truncated_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = make_ctx(dir.path());
        let command = if shell_is_unixy() {
            "seq 1 10000"
        } else {
            "for /L %i in (1,1,2000) do @echo 0123456789abcdef"
        };
        let out = BashTool
            .call(serde_json::json!({"command": command}), &mut ctx)
            .await
            .unwrap();
        assert!(out.truncated, "{}", out.content);
        assert!(
            out.content.contains("full output saved to"),
            "{}",
            out.content
        );
        let path = out.full_output_path.expect("full output path recorded");
        let full = std::fs::read_to_string(&path).unwrap();
        assert!(full.len() > MAX_OUTPUT_CHARS);
    }
}
