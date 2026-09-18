pub mod background;
pub mod fs;
pub mod notebook;
pub mod patch;
pub mod plan;
pub mod sandbox;
pub mod search;
pub mod shell;
pub mod task;
pub mod todo;
pub mod webfetch;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

pub use background::{BackgroundTaskRegistry, TaskOutputTool, TaskStopTool};
pub use fs::{FileEdit, FileRead, FileReadRecord, FileWrite, ReadFileState};
pub use notebook::NotebookEdit;
pub use patch::ApplyPatch;
pub use plan::{EnterPlanMode, ExitPlanMode};
pub use search::{GlobTool, GrepTool};
pub use shell::BashTool;
pub use task::TaskTool;
pub use todo::{render_todos, TodoWrite};
pub use webfetch::WebFetch;

/// 工具执行的统一输出。
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    pub truncated: bool,
    /// 大输出落盘位置（如 Bash 超长输出）。
    pub full_output_path: Option<PathBuf>,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            truncated: false,
            full_output_path: None,
        }
    }

    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            truncated: false,
            full_output_path: None,
        }
    }
}

/// 工具执行上下文，跨调用共享。
pub struct ToolContext {
    pub working_dir: PathBuf,
    pub read_state: ReadFileState,
    /// 大输出落盘目录，调用方负责创建。
    pub output_dir: PathBuf,
    pub session_id: String,
    /// 会话级任务清单（TodoWrite 维护），由 AgentRuntime 与 Session 同步。
    pub todos: Vec<crate::model::TodoItem>,
    /// 后台任务注册表（Bash run_in_background / TaskOutput / TaskStop）。
    pub background: BackgroundTaskRegistry,
    /// 当前 run 的生效权限模式（EnterPlanMode / ExitPlanMode 可切换）。
    pub mode: crate::permissions::PermissionMode,
    /// 进入计划模式前的模式，供 ExitPlanMode 恢复。
    pub pre_plan_mode: Option<crate::permissions::PermissionMode>,
}

impl ToolContext {
    /// 相对路径基于 working_dir 解析为绝对路径。
    pub fn resolve_path(&self, path: &str) -> PathBuf {
        let p = std::path::Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.working_dir.join(p)
        }
    }

    /// 测试与子代理用的最小构造器：默认 Default 权限模式、空后台注册表。
    pub fn for_tests(working_dir: PathBuf) -> Self {
        Self {
            working_dir: working_dir.clone(),
            read_state: ReadFileState::new(),
            output_dir: working_dir.join("out"),
            session_id: "test".to_string(),
            todos: Vec::new(),
            background: BackgroundTaskRegistry::new(),
            mode: crate::permissions::PermissionMode::Default,
            pre_plan_mode: None,
        }
    }
}

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    /// 给模型看的工具说明。
    fn description(&self) -> &str;
    /// JSON Schema object。
    fn input_schema(&self) -> serde_json::Value;
    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        false
    }
    /// 永远免权限提示的工具（如 TodoWrite 只改会话内部状态）。
    fn is_always_allowed(&self) -> bool {
        false
    }
    /// 供权限系统匹配的内容，如 Bash 返回拆分后的命令列表。
    fn rule_contents(&self, _input: &serde_json::Value) -> Vec<String> {
        Vec::new()
    }
    /// 权限评估涉及的目标路径（供 .git/ 安全检查）。
    fn target_paths(&self, _input: &serde_json::Value, _ctx: &ToolContext) -> Vec<PathBuf> {
        Vec::new()
    }
    async fn call(&self, input: serde_json::Value, ctx: &mut ToolContext) -> Result<ToolOutput>;
}

#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: Arc<std::sync::RwLock<Vec<Arc<dyn Tool>>>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册全部内置工具。`directory` 供 Task 工具委派子代理。
    pub fn builtin_with_directory(directory: crate::collaboration::AgentDirectory) -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(FileRead));
        registry.register(Arc::new(FileWrite));
        registry.register(Arc::new(FileEdit));
        registry.register(Arc::new(GlobTool));
        registry.register(Arc::new(GrepTool));
        registry.register(Arc::new(BashTool));
        registry.register(Arc::new(WebFetch));
        registry.register(Arc::new(ApplyPatch));
        registry.register(Arc::new(NotebookEdit));
        registry.register(Arc::new(TaskOutputTool));
        registry.register(Arc::new(TaskStopTool));
        registry.register(Arc::new(EnterPlanMode));
        registry.register(Arc::new(ExitPlanMode));
        registry.register(Arc::new(TaskTool::new(directory)));
        registry.register(Arc::new(TodoWrite));
        registry
    }

    /// 注册除 Task / TodoWrite 之外的内置工具（测试与无目录场景）。
    pub fn builtin() -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(FileRead));
        registry.register(Arc::new(FileWrite));
        registry.register(Arc::new(FileEdit));
        registry.register(Arc::new(GlobTool));
        registry.register(Arc::new(GrepTool));
        registry.register(Arc::new(BashTool));
        registry.register(Arc::new(WebFetch));
        registry.register(Arc::new(ApplyPatch));
        registry.register(Arc::new(NotebookEdit));
        registry.register(Arc::new(TaskOutputTool));
        registry.register(Arc::new(TaskStopTool));
        registry.register(Arc::new(EnterPlanMode));
        registry.register(Arc::new(ExitPlanMode));
        registry
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(tool);
    }

    pub fn find(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.iter().find(|t| t.name() == name)
    }

    pub fn iter(&self) -> impl Iterator<Item = Arc<dyn Tool>> {
        self.tools
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .into_iter()
    }

    pub fn replace_mcp(&self, tools: Vec<Arc<dyn Tool>>) {
        let mut all = self.tools.write().unwrap_or_else(|e| e.into_inner());
        all.retain(|tool| !tool.name().starts_with(crate::mcp::MCP_TOOL_PREFIX));
        all.extend(tools);
    }

    /// 中立的工具定义列表，可直接填入 `ModelRequest::tools`。
    pub fn tool_definitions(&self) -> Vec<crate::provider::ToolDefinition> {
        self.iter()
            .map(|tool| crate::provider::ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                input_schema: tool.input_schema(),
            })
            .collect()
    }

    /// OpenAI tools 格式：`{"type":"function","function":{"name","description","parameters"}}`。
    pub fn openai_tool_definitions(&self) -> Vec<serde_json::Value> {
        self.iter()
            .map(|tool| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": tool.name(),
                        "description": tool.description(),
                        "parameters": tool.input_schema(),
                    }
                })
            })
            .collect()
    }
}

/// 将字符串截断到不超过 `max` 字节，保证落在 char 边界上。
pub(crate) fn truncate_str(s: &mut String, max: usize) {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_output_constructors() {
        let ok = ToolOutput::ok("done");
        assert!(!ok.is_error && !ok.truncated && ok.full_output_path.is_none());
        let err = ToolOutput::err("boom");
        assert!(err.is_error);
    }

    #[test]
    fn builtin_registry_contains_all_tools() {
        let registry = ToolRegistry::builtin();
        for name in [
            "FileRead",
            "FileWrite",
            "FileEdit",
            "Glob",
            "Grep",
            "Bash",
            "WebFetch",
            "ApplyPatch",
            "NotebookEdit",
            "TaskOutput",
            "TaskStop",
            "EnterPlanMode",
            "ExitPlanMode",
        ] {
            assert!(registry.find(name).is_some(), "missing tool {name}");
        }
        assert_eq!(registry.iter().count(), 13);
    }

    #[test]
    fn builtin_with_directory_registers_task_and_todo() {
        let registry =
            ToolRegistry::builtin_with_directory(crate::collaboration::AgentDirectory::new());
        assert!(registry.find("Task").is_some());
        assert!(registry.find("TodoWrite").is_some());
        assert!(registry.find("WebFetch").is_some());
        assert!(registry.find("TodoWrite").unwrap().is_always_allowed());
        assert!(!registry.find("Bash").unwrap().is_always_allowed());
        // 模式切换工具免权限；后台查看只读。
        assert!(registry.find("EnterPlanMode").unwrap().is_always_allowed());
        assert!(registry.find("ExitPlanMode").unwrap().is_always_allowed());
        assert!(registry
            .find("TaskOutput")
            .unwrap()
            .is_read_only(&serde_json::json!({})));
    }

    #[test]
    fn tool_definitions_shape() {
        let registry = ToolRegistry::builtin();
        let defs = registry.tool_definitions();
        assert_eq!(defs.len(), 13);
        for def in &defs {
            assert!(!def.name.is_empty());
            assert!(!def.description.is_empty());
            assert_eq!(def.input_schema["type"], "object");
        }
    }

    #[test]
    fn openai_tool_definitions_shape() {
        let registry = ToolRegistry::builtin();
        let defs = registry.openai_tool_definitions();
        assert_eq!(defs.len(), 13);
        for def in &defs {
            assert_eq!(def["type"], "function");
            assert!(def["function"]["name"].is_string());
            assert!(def["function"]["description"].is_string());
            assert_eq!(def["function"]["parameters"]["type"], "object");
        }
    }

    #[test]
    fn resolve_path_relative_and_absolute() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::for_tests(dir.path().to_path_buf());
        assert_eq!(ctx.resolve_path("a/b.txt"), dir.path().join("a/b.txt"));
        let abs = if cfg!(windows) { "C:/tmp/x" } else { "/tmp/x" };
        assert_eq!(ctx.resolve_path(abs), PathBuf::from(abs));
    }
}
