use anyhow::Result;
use serde_json::Value;

use super::{Tool, ToolContext, ToolOutput};
use crate::model::{TodoItem, TodoStatus};

/// 单次写入的任务数量上限，防止模型生成超长清单。
const MAX_TODOS: usize = 50;
/// 单条任务内容长度上限（字符）。
const MAX_CONTENT_CHARS: usize = 200;

fn parse_status(value: &Value) -> Option<TodoStatus> {
    match value.as_str()? {
        "pending" => Some(TodoStatus::Pending),
        "in_progress" => Some(TodoStatus::InProgress),
        "completed" => Some(TodoStatus::Completed),
        _ => None,
    }
}

/// 渲染当前任务清单，供系统提示词与工具输出共用。
pub fn render_todos(todos: &[TodoItem]) -> String {
    todos
        .iter()
        .enumerate()
        .map(|(index, todo)| format!("{}. {}", index + 1, todo.render()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 会话级任务清单工具，对应 Claude Code 的 TodoWrite：
/// 模型在多步任务中主动创建/更新清单，状态持久化到 Session，
/// 并作为动态段注入系统提示词（压缩后也不丢失）。
pub struct TodoWrite;

#[async_trait::async_trait]
impl Tool for TodoWrite {
    fn name(&self) -> &'static str {
        "TodoWrite"
    }

    fn description(&self) -> &'static str {
        "Update the todo list for the current session. Use it proactively for complex \
         multi-step tasks (3+ steps) to track progress: mark a task in_progress BEFORE \
         starting it, completed IMMEDIATELY after finishing it, and keep exactly one \
         task in_progress at a time. Each item needs `content` (imperative form, e.g. \
         \"Run tests\") and `activeForm` (present continuous form, e.g. \"Running tests\"). \
         Skip this tool for single trivial tasks. The list replaces the previous one \
         entirely; drop items that are no longer relevant."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "The complete todo list (replaces the previous one)",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {
                                "type": "string",
                                "description": "Imperative task description, e.g. \"Run tests\""
                            },
                            "activeForm": {
                                "type": "string",
                                "description": "Present continuous form, e.g. \"Running tests\""
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"],
                                "description": "Task status"
                            }
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["todos"]
        })
    }

    /// 只修改会话内部状态，借鉴 Claude Code 对 TodoWrite 免权限提示的处理。
    fn is_always_allowed(&self) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: &mut ToolContext) -> Result<ToolOutput> {
        let Some(list) = input.get("todos").and_then(|v| v.as_array()) else {
            return Ok(ToolOutput::err("missing required parameter: todos (array)"));
        };
        if list.len() > MAX_TODOS {
            return Ok(ToolOutput::err(format!(
                "too many todos: {} (max {MAX_TODOS})",
                list.len()
            )));
        }

        let mut todos = Vec::with_capacity(list.len());
        for (index, item) in list.iter().enumerate() {
            let Some(content) = item.get("content").and_then(|v| v.as_str()) else {
                return Ok(ToolOutput::err(format!(
                    "todos[{index}]: missing required field: content"
                )));
            };
            let content = content.trim();
            if content.is_empty() {
                return Ok(ToolOutput::err(format!("todos[{index}]: content is empty")));
            }
            let Some(status_value) = item.get("status") else {
                return Ok(ToolOutput::err(format!(
                    "todos[{index}]: missing required field: status"
                )));
            };
            let Some(status) = parse_status(status_value) else {
                return Ok(ToolOutput::err(format!(
                    "todos[{index}]: invalid status {:?} (pending / in_progress / completed)",
                    status_value
                )));
            };
            let mut content = content.to_string();
            if content.chars().count() > MAX_CONTENT_CHARS {
                content = content.chars().take(MAX_CONTENT_CHARS).collect();
            }
            todos.push(TodoItem {
                content,
                active_form: item
                    .get("activeForm")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                status,
            });
        }

        let in_progress = todos
            .iter()
            .filter(|todo| todo.status == TodoStatus::InProgress)
            .count();
        ctx.todos = todos;
        let body = if ctx.todos.is_empty() {
            "Todo list cleared.".to_string()
        } else {
            render_todos(&ctx.todos)
        };
        let note = if in_progress > 1 {
            format!("\n\nWarning: {in_progress} tasks are in_progress; keep exactly one at a time.")
        } else {
            String::new()
        };
        Ok(ToolOutput::ok(format!(
            "Todo list updated ({in_progress} in_progress):\n{body}{note}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> ToolContext {
        ToolContext {
            working_dir: std::env::temp_dir(),
            read_state: crate::tools::ReadFileState::new(),
            output_dir: std::env::temp_dir().join("tool-results"),
            session_id: "s".to_string(),
            todos: Vec::new(),
            background: crate::tools::background::BackgroundTaskRegistry::new(),
            mode: crate::permissions::PermissionMode::Default,
            pre_plan_mode: None,
        }
    }

    #[tokio::test]
    async fn todo_write_replaces_list_and_renders() {
        let mut context = ctx();
        let output = TodoWrite
            .call(
                json!({
                    "todos": [
                        {"content": "Run tests", "activeForm": "Running tests", "status": "in_progress"},
                        {"content": "Build the project", "status": "pending"}
                    ]
                }),
                &mut context,
            )
            .await
            .unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("Running tests"));
        assert!(output.content.contains("Build the project"));
        assert_eq!(context.todos.len(), 2);
        assert_eq!(context.todos[0].status, TodoStatus::InProgress);
        assert_eq!(
            context.todos[0].active_form.as_deref(),
            Some("Running tests")
        );
        assert_eq!(context.todos[1].status, TodoStatus::Pending);

        // 整体替换：写入空清单即清空。
        let output = TodoWrite
            .call(json!({"todos": []}), &mut context)
            .await
            .unwrap();
        assert!(output.content.contains("cleared"));
        assert!(context.todos.is_empty());
    }

    #[tokio::test]
    async fn todo_write_validates_input() {
        let mut context = ctx();
        for bad in [
            json!({"todos": [{"content": "x", "status": "done"}]}),
            json!({"todos": [{"status": "pending"}]}),
            json!({"todos": [{"content": "  ", "status": "pending"}]}),
            json!({"todos": "nope"}),
        ] {
            let output = TodoWrite.call(bad.clone(), &mut context).await.unwrap();
            assert!(output.is_error, "expected error for {bad}");
        }
        assert!(context.todos.is_empty());

        let too_many = json!({
            "todos": (0..MAX_TODOS + 1)
                .map(|i| json!({"content": format!("task {i}"), "status": "pending"}))
                .collect::<Vec<_>>()
        });
        let output = TodoWrite.call(too_many, &mut context).await.unwrap();
        assert!(output.is_error);
    }

    #[tokio::test]
    async fn multiple_in_progress_warns() {
        let mut context = ctx();
        let output = TodoWrite
            .call(
                json!({
                    "todos": [
                        {"content": "a", "status": "in_progress"},
                        {"content": "b", "status": "in_progress"}
                    ]
                }),
                &mut context,
            )
            .await
            .unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("Warning: 2 tasks are in_progress"));
    }

    #[test]
    fn render_todos_enumerates_and_marks_status() {
        let todos = vec![
            TodoItem {
                content: "first".to_string(),
                active_form: None,
                status: TodoStatus::Completed,
            },
            TodoItem {
                content: "second".to_string(),
                active_form: Some("Doing second".to_string()),
                status: TodoStatus::InProgress,
            },
        ];
        let rendered = render_todos(&todos);
        assert!(rendered.contains("1. ☑ [completed] first"));
        assert!(rendered.contains("2. ◐ [in_progress] Doing second"));
        assert!(rendered.contains(TodoStatus::Completed.label()));
    }
}
