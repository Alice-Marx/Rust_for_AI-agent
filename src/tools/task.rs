use anyhow::Result;
use serde_json::Value;

use crate::collaboration::AgentDirectory;

use super::{Tool, ToolContext, ToolOutput};

/// 模型侧子代理委派工具，对应 Claude Code 的 Task 工具：
/// 把一个子任务交给已注册的命名 Agent 执行并返回其输出。
/// 权限规则按 Agent 名匹配：`Task(agent:research)`。
pub struct TaskTool {
    directory: AgentDirectory,
}

impl TaskTool {
    pub fn new(directory: AgentDirectory) -> Self {
        Self { directory }
    }
}

#[async_trait::async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &'static str {
        "Task"
    }

    fn description(&self) -> &'static str {
        "Delegate a self-contained subtask to a registered sub-agent and return its \
         output. Use it for parallelizable or specialized work (e.g. research, \
         expense analysis) instead of doing everything yourself. The sub-agent has \
         no access to this conversation; the task description must be complete and \
         standalone. Call with an unknown agent name to discover the registered ones."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "description": "Name of the registered sub-agent, e.g. \"research\""
                },
                "task": {
                    "type": "string",
                    "description": "Complete, standalone description of the subtask"
                }
            },
            "required": ["agent", "task"]
        })
    }

    fn rule_contents(&self, input: &Value) -> Vec<String> {
        input
            .get("agent")
            .and_then(|v| v.as_str())
            .map(|agent| vec![format!("agent:{agent}")])
            .unwrap_or_default()
    }

    async fn call(&self, input: Value, _ctx: &mut ToolContext) -> Result<ToolOutput> {
        let Some(agent) = input.get("agent").and_then(|v| v.as_str()) else {
            return Ok(ToolOutput::err("missing required parameter: agent"));
        };
        let Some(task) = input.get("task").and_then(|v| v.as_str()) else {
            return Ok(ToolOutput::err("missing required parameter: task"));
        };
        let task = task.trim();
        if task.is_empty() {
            return Ok(ToolOutput::err("task must not be empty"));
        }

        match self.directory.call(agent, task).await {
            Ok(output) => Ok(ToolOutput::ok(format!(
                "Sub-agent '{agent}' completed the task:\n\n{output}"
            ))),
            Err(error) => {
                let names = self.directory.names().await.join(", ");
                Ok(ToolOutput::err(format!(
                    "sub-agent '{agent}' failed: {error:#}\nregistered agents: [{names}]"
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collaboration::EchoAgent;
    use serde_json::json;
    use std::sync::Arc;

    fn ctx() -> ToolContext {
        ToolContext {
            working_dir: std::env::temp_dir(),
            read_state: crate::tools::ReadFileState::new(),
            output_dir: std::env::temp_dir().join("out"),
            session_id: "s".to_string(),
            todos: Vec::new(),
            background: crate::tools::background::BackgroundTaskRegistry::new(),
            mode: crate::permissions::PermissionMode::Default,
            pre_plan_mode: None,
        }
    }

    #[tokio::test]
    async fn task_tool_delegates_to_registered_agent() {
        let directory = AgentDirectory::new();
        directory
            .register(Arc::new(EchoAgent {
                agent_name: "helper".to_string(),
            }))
            .await;
        let tool = TaskTool::new(directory);
        let mut context = ctx();

        let output = tool
            .call(
                json!({"agent": "helper", "task": "summarize the repo"}),
                &mut context,
            )
            .await
            .unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("helper"));
        assert!(output.content.contains("summarize the repo"));
    }

    #[tokio::test]
    async fn unknown_agent_lists_registered_names() {
        let directory = AgentDirectory::new();
        directory
            .register(Arc::new(EchoAgent {
                agent_name: "research".to_string(),
            }))
            .await;
        let tool = TaskTool::new(directory);
        let mut context = ctx();

        let output = tool
            .call(json!({"agent": "nope", "task": "x"}), &mut context)
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("registered agents: [research]"));
    }

    #[test]
    fn rule_content_is_agent_name() {
        let tool = TaskTool::new(AgentDirectory::new());
        let content = tool.rule_contents(&json!({"agent": "research", "task": "x"}));
        assert_eq!(content, vec!["agent:research".to_string()]);
        assert!(tool.rule_contents(&json!({"task": "x"})).is_empty());
    }
}
