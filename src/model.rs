use serde::{Deserialize, Serialize};

use crate::{
    evaluation::EvaluationReport,
    memory::MemoryMatch,
    permissions::PermissionMode,
    planning::{Plan, Reflection},
    provider::Usage,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRequest {
    pub session_id: String,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Explicitly enable local project skills by their directory name. Skills
    /// not named here may still be selected from their `when_to_use` metadata.
    #[serde(default)]
    pub skills: Vec<String>,
    /// 权限模式；缺省为 `PermissionMode::Default`。
    #[serde(default)]
    pub mode: Option<PermissionMode>,
    /// 工具执行的工作目录；缺省为服务端当前目录。
    #[serde(default)]
    pub cwd: Option<String>,
    /// 本次请求的推理档位覆盖：low / medium / high（off 或 none 表示关闭）。
    /// 只有模型能力档案声明支持推理时才会真正下发。
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    pub input: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResponse {
    pub execution_id: String,
    pub session_id: String,
    pub output: String,
    pub plan: Plan,
    pub reflection: Reflection,
    pub delegated: Vec<DelegatedResult>,
    pub skills: Vec<crate::skills::ActivatedSkill>,
    pub memories: Vec<MemoryMatch>,
    pub evaluation: EvaluationReport,
    /// 本轮执行的工具调用轮数（每轮 = 一次模型响应中的全部工具调用）。
    #[serde(default)]
    pub turns: usize,
    /// 本轮执行的工具调用总次数。
    #[serde(default)]
    pub tool_calls: usize,
    /// 本次 run 累计的 token 用量。
    #[serde(default)]
    pub usage: Usage,
    /// 本次 run 结束时的任务清单状态（由 TodoWrite 工具维护）。
    #[serde(default)]
    pub todos: Vec<TodoItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegatedResult {
    pub agent: String,
    pub task: String,
    pub output: String,
}

/// TodoWrite 工具维护的单条任务，对应 Claude Code 的 todo 结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    /// 祈使句形式的任务描述，如 "Run tests"。
    pub content: String,
    /// 进行时形式，展示执行进度时使用，如 "Running tests"。
    #[serde(default)]
    pub active_form: Option<String>,
    pub status: TodoStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    /// 展示用的标记，与 Claude Code 的 todo 渲染一致。
    pub fn label(self) -> &'static str {
        match self {
            TodoStatus::Pending => "☐",
            TodoStatus::InProgress => "◐",
            TodoStatus::Completed => "☑",
        }
    }
}

impl TodoItem {
    /// 渲染为系统提示词 / CLI 展示用的一行文本。
    pub fn render(&self) -> String {
        let active = self.active_form.as_deref().unwrap_or(&self.content);
        format!(
            "{} [{}] {}",
            self.status.label(),
            status_name(self.status),
            active
        )
    }
}

fn status_name(status: TodoStatus) -> &'static str {
    match status {
        TodoStatus::Pending => "pending",
        TodoStatus::InProgress => "in_progress",
        TodoStatus::Completed => "completed",
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryWriteRequest {
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    pub content: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_importance")]
    pub importance: f32,
}

fn default_importance() -> f32 {
    0.5
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxRequest {
    pub language: String,
    pub code: String,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}
