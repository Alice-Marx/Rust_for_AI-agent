use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::watch;

use super::shell::default_shell;
use super::{Tool, ToolContext, ToolOutput};

/// 后台任务输出缓冲上限（字符），超出后丢弃尾部并追加提示。
const MAX_BUFFERED_CHARS: usize = 200_000;
/// TaskOutput 默认等待时长与上限。
const DEFAULT_WAIT_MS: u64 = 30_000;
const MAX_WAIT_MS: u64 = 600_000;

/// 单个后台任务的共享状态。
#[derive(Debug, Clone)]
enum TaskStatus {
    Running,
    Completed(Option<i32>),
    Failed(String),
    Killed,
}

impl TaskStatus {
    fn label(&self) -> String {
        match self {
            TaskStatus::Running => "running".to_string(),
            TaskStatus::Completed(Some(code)) => format!("completed (exit code {code})"),
            TaskStatus::Completed(None) => "completed".to_string(),
            TaskStatus::Failed(error) => format!("failed: {error}"),
            TaskStatus::Killed => "stopped by TaskStop".to_string(),
        }
    }

    fn is_finished(&self) -> bool {
        !matches!(self, TaskStatus::Running)
    }
}

#[derive(Clone)]
struct BackgroundTask {
    command: String,
    status: Arc<Mutex<TaskStatus>>,
    output: Arc<Mutex<String>>,
    kill: watch::Sender<bool>,
}

/// 后台任务注册表：Bash `run_in_background` 启动的任务由 TaskOutput /
/// TaskStop 工具后续访问。克隆共享同一底层状态。
#[derive(Clone, Default)]
pub struct BackgroundTaskRegistry {
    tasks: Arc<Mutex<HashMap<String, BackgroundTask>>>,
}

impl BackgroundTaskRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.lock().unwrap().is_empty()
    }

    fn insert(&self, id: String, task: BackgroundTask) {
        self.tasks.lock().unwrap().insert(id, task);
    }

    fn get(&self, id: &str) -> Option<BackgroundTask> {
        self.tasks.lock().unwrap().get(id).cloned()
    }

    /// 把命令放入后台执行，返回任务 id。
    pub async fn spawn(&self, command: &str, cwd: &Path) -> Result<String> {
        let id = format!("bg-{}", uuid::Uuid::new_v4());
        let (shell, args) = default_shell();
        let mut child = Command::new(&shell)
            .args(&args)
            .arg(command)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();
        let status = Arc::new(Mutex::new(TaskStatus::Running));
        let output = Arc::new(Mutex::new(String::new()));
        let (kill_tx, mut kill_rx) = watch::channel(false);

        let status_clone = status.clone();
        let output_clone = output.clone();
        tokio::spawn(async move {
            let stdout_task = pump_output(stdout_pipe, output_clone.clone());
            let stderr_task = pump_output(stderr_pipe, output_clone);
            let exit = tokio::select! {
                waited = child.wait() => match waited {
                    Ok(code) => TaskStatus::Completed(code.code()),
                    Err(error) => TaskStatus::Failed(format!("wait failed: {error}")),
                },
                _ = kill_rx.changed() => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    TaskStatus::Killed
                }
            };
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            *status_clone.lock().unwrap() = exit;
        });

        self.insert(
            id.clone(),
            BackgroundTask {
                command: command.to_string(),
                status,
                output,
                kill: kill_tx,
            },
        );
        Ok(id)
    }
}

/// 持续读取管道并追加到共享缓冲；超出上限后丢弃尾部。
async fn pump_output(
    mut pipe: Option<impl tokio::io::AsyncRead + Unpin>,
    buffer: Arc<Mutex<String>>,
) {
    let Some(pipe) = pipe.as_mut() else {
        return;
    };
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut buf = buffer.lock().unwrap();
                if buf.len() < MAX_BUFFERED_CHARS {
                    buf.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    if buf.len() > MAX_BUFFERED_CHARS {
                        let mut cut = MAX_BUFFERED_CHARS;
                        while !buf.is_char_boundary(cut) {
                            cut -= 1;
                        }
                        buf.truncate(cut);
                        buf.push_str("\n... (output truncated)");
                    }
                }
            }
        }
    }
}

/// 后台任务输出查看工具，对应 Claude Code 的 TaskOutput（旧名 BashOutput）。
pub struct TaskOutputTool;

#[async_trait::async_trait]
impl Tool for TaskOutputTool {
    fn name(&self) -> &str {
        "TaskOutput"
    }

    fn description(&self) -> &str {
        "Retrieve output from a background task started with Bash(run_in_background=true). \
         Set block=true (default) to wait up to timeout_ms for completion; set block=false \
         to poll current status immediately."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string", "description": "Background task id (bg-...)"},
                "block": {"type": "boolean", "description": "Wait for completion (default true)"},
                "timeout_ms": {
                    "type": "integer",
                    "description": "Max wait in milliseconds when block=true (default 30000, max 600000)"
                }
            },
            "required": ["task_id"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }

    async fn call(&self, input: serde_json::Value, ctx: &mut ToolContext) -> Result<ToolOutput> {
        let Some(task_id) = input.get("task_id").and_then(|v| v.as_str()) else {
            return Ok(ToolOutput::err("missing required parameter: task_id"));
        };
        let block = input.get("block").and_then(|v| v.as_bool()).unwrap_or(true);
        let wait_ms = input
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_WAIT_MS)
            .min(MAX_WAIT_MS);

        let Some(task) = ctx.background.get(task_id) else {
            return Ok(ToolOutput::err(format!("Unknown task_id: {task_id}")));
        };

        let deadline = tokio::time::Instant::now() + Duration::from_millis(wait_ms);
        loop {
            let (status_label, finished) = {
                let status = task.status.lock().unwrap();
                (status.label(), status.is_finished())
            };
            if finished || !block {
                let output = task.output.lock().unwrap().clone();
                let mut content = format!(
                    "Task {task_id} [{status_label}]\ncommand: {}\n\n{}",
                    task.command,
                    if output.is_empty() {
                        "(no output yet)"
                    } else {
                        &output
                    }
                );
                if !finished && !block {
                    content.push_str(
                        "\n\n(retrieval_status: not_finished; call again with block=true to wait)",
                    );
                }
                return Ok(ToolOutput::ok(content));
            }
            if tokio::time::Instant::now() >= deadline {
                let output = task.output.lock().unwrap().clone();
                let mut content = format!(
                    "Task {task_id} still [running]\ncommand: {}\n\n{}",
                    task.command,
                    if output.is_empty() {
                        "(no output yet)"
                    } else {
                        &output
                    }
                );
                content.push_str(
                    "\n\n(retrieval_status: timeout; call TaskOutput again to keep waiting)",
                );
                return Ok(ToolOutput::ok(content));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

/// 后台任务停止工具，对应 Claude Code 的 TaskStop（旧名 KillShell）。
pub struct TaskStopTool;

#[async_trait::async_trait]
impl Tool for TaskStopTool {
    fn name(&self) -> &str {
        "TaskStop"
    }

    fn description(&self) -> &str {
        "Stop a running background task started with Bash(run_in_background=true)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string", "description": "Background task id (bg-...)"}
            },
            "required": ["task_id"]
        })
    }

    async fn call(&self, input: serde_json::Value, ctx: &mut ToolContext) -> Result<ToolOutput> {
        let Some(task_id) = input.get("task_id").and_then(|v| v.as_str()) else {
            return Ok(ToolOutput::err("missing required parameter: task_id"));
        };
        let Some(task) = ctx.background.get(task_id) else {
            return Ok(ToolOutput::err(format!("Unknown task_id: {task_id}")));
        };
        if task.status.lock().unwrap().is_finished() {
            return Ok(ToolOutput::err(format!(
                "Task {task_id} is not running (status: {})",
                task.status.lock().unwrap().label()
            )));
        }
        let _ = task.kill.send(true);
        // 等待进程真正退出（最多 5 秒），保证后续 TaskOutput 能读到完整输出。
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !task.status.lock().unwrap().is_finished() {
            if tokio::time::Instant::now() >= deadline {
                return Ok(ToolOutput::ok(format!(
                    "Task {task_id} stop requested; the process is still terminating."
                )));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(ToolOutput::ok(format!(
            "Task {task_id} stopped (status: {}).",
            task.status.lock().unwrap().label()
        )))
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
            background: BackgroundTaskRegistry::new(),
            mode: crate::permissions::PermissionMode::Default,
            pre_plan_mode: None,
        }
    }

    fn shell_is_unixy() -> bool {
        let (shell, _) = default_shell();
        shell.contains("bash") || shell.ends_with("sh")
    }

    #[tokio::test]
    async fn background_bash_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = make_ctx(dir.path());
        let sleep_cmd = if shell_is_unixy() {
            "echo started-bg; sleep 30"
        } else {
            "echo started-bg & ping -n 30 127.0.0.1 >nul"
        };
        let id = ctx.background.spawn(sleep_cmd, dir.path()).await.unwrap();

        // block=false 立即返回 running 状态。
        let poll = TaskOutputTool
            .call(serde_json::json!({"task_id": id, "block": false}), &mut ctx)
            .await
            .unwrap();
        assert!(!poll.is_error);

        // TaskStop 停止任务。
        let stop = TaskStopTool
            .call(serde_json::json!({"task_id": id}), &mut ctx)
            .await
            .unwrap();
        assert!(!stop.is_error, "{}", stop.content);

        // 再次停止报错：任务已不在运行。
        let stop_again = TaskStopTool
            .call(serde_json::json!({"task_id": id}), &mut ctx)
            .await
            .unwrap();
        assert!(stop_again.is_error, "{}", stop_again.content);
    }

    #[tokio::test]
    async fn task_output_returns_finished_output() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = make_ctx(dir.path());
        let id = ctx
            .background
            .spawn("echo bg-done-marker", dir.path())
            .await
            .unwrap();

        let out = TaskOutputTool
            .call(
                serde_json::json!({"task_id": id, "block": true, "timeout_ms": 10000}),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("bg-done-marker"), "{}", out.content);
        assert!(out.content.contains("completed"), "{}", out.content);
    }

    #[tokio::test]
    async fn unknown_task_id_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = make_ctx(dir.path());
        let out = TaskOutputTool
            .call(serde_json::json!({"task_id": "bg-nope"}), &mut ctx)
            .await
            .unwrap();
        assert!(out.is_error);
    }
}
