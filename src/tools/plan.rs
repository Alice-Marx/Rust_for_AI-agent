use anyhow::Result;

use super::{Tool, ToolContext, ToolOutput};
use crate::permissions::PermissionMode;

/// 进入计划模式，对应 Claude Code 的 EnterPlanMode：把当前 run 的权限
/// 模式切换为 Plan（只读），后续工具调用中的写入类操作会被权限系统拒绝。
/// 工具本身免权限、只读。
pub struct EnterPlanMode;

#[async_trait::async_trait]
impl Tool for EnterPlanMode {
    fn name(&self) -> &str {
        "EnterPlanMode"
    }

    fn description(&self) -> &str {
        "Switch this task into plan mode before starting a non-trivial implementation: \
         explore the codebase in read-only mode, design an approach, and present it to \
         the user for approval. In plan mode all write tools are denied. Call \
         ExitPlanMode (after presenting the plan in your final answer) when the plan \
         is ready for review."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn is_always_allowed(&self) -> bool {
        true
    }

    async fn call(&self, _input: serde_json::Value, ctx: &mut ToolContext) -> Result<ToolOutput> {
        if ctx.mode == PermissionMode::Plan {
            return Ok(ToolOutput::ok("Already in plan mode."));
        }
        ctx.pre_plan_mode.get_or_insert(ctx.mode);
        ctx.mode = PermissionMode::Plan;
        Ok(ToolOutput::ok(
            "Plan mode is now active: write tools are denied for the rest of this task. \
             Explore the codebase, design an approach, then present the plan in your \
             final answer and call ExitPlanMode.",
        ))
    }
}

/// 退出计划模式，对应 Claude Code 的 ExitPlanMode：把权限模式恢复为
/// 进入计划前的值（缺省为请求自带的模式）。`allowed_prompts` 会被记录
/// 在返回结果里，提示交互式前端在批准计划时可以顺带放行的操作。
pub struct ExitPlanMode;

#[async_trait::async_trait]
impl Tool for ExitPlanMode {
    fn name(&self) -> &str {
        "ExitPlanMode"
    }

    fn description(&self) -> &str {
        "Leave plan mode and restore the previous permission mode. Call this only after \
         you have presented the full plan in your final answer for user review. \
         Optionally list semantic Bash prompts (e.g. \"run tests\") the user may want \
         to approve together with the plan."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "allowed_prompts": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Semantic Bash prompts to request alongside plan approval, e.g. [\"run tests\"]"
                }
            },
            "required": []
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn is_always_allowed(&self) -> bool {
        true
    }

    async fn call(&self, input: serde_json::Value, ctx: &mut ToolContext) -> Result<ToolOutput> {
        let restore = ctx.pre_plan_mode.take().unwrap_or(PermissionMode::Default);
        ctx.mode = restore;
        let allowed = input
            .get("allowed_prompts")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        if allowed.is_empty() {
            Ok(ToolOutput::ok(format!(
                "Exited plan mode; permission mode restored to {restore:?}. The plan in \
                 your final answer is ready for user review."
            )))
        } else {
            Ok(ToolOutput::ok(format!(
                "Exited plan mode; permission mode restored to {restore:?}. Requested \
                 allowed prompts for plan approval: [{allowed}]. The plan in your final \
                 answer is ready for user review."
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn make_ctx(dir: &Path, mode: PermissionMode) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            read_state: crate::tools::ReadFileState::new(),
            output_dir: dir.join("out"),
            session_id: "test".to_string(),
            todos: Vec::new(),
            background: super::super::background::BackgroundTaskRegistry::new(),
            mode,
            pre_plan_mode: None,
        }
    }

    #[tokio::test]
    async fn enter_and_exit_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = make_ctx(dir.path(), PermissionMode::BypassPermissions);

        let out = EnterPlanMode
            .call(serde_json::json!({}), &mut ctx)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(ctx.mode, PermissionMode::Plan);
        assert_eq!(ctx.pre_plan_mode, Some(PermissionMode::BypassPermissions));

        let out = ExitPlanMode
            .call(
                serde_json::json!({"allowed_prompts": ["run tests"]}),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(ctx.mode, PermissionMode::BypassPermissions);
        assert_eq!(ctx.pre_plan_mode, None);
        assert!(out.content.contains("run tests"));
    }

    #[tokio::test]
    async fn enter_twice_keeps_original_mode() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = make_ctx(dir.path(), PermissionMode::AcceptEdits);

        EnterPlanMode
            .call(serde_json::json!({}), &mut ctx)
            .await
            .unwrap();
        // 第二次进入不改写 pre_plan_mode。
        EnterPlanMode
            .call(serde_json::json!({}), &mut ctx)
            .await
            .unwrap();
        assert_eq!(ctx.pre_plan_mode, Some(PermissionMode::AcceptEdits));

        ExitPlanMode
            .call(serde_json::json!({}), &mut ctx)
            .await
            .unwrap();
        assert_eq!(ctx.mode, PermissionMode::AcceptEdits);
    }

    #[tokio::test]
    async fn exit_without_enter_restores_default() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = make_ctx(dir.path(), PermissionMode::Plan);
        ctx.pre_plan_mode = None;

        ExitPlanMode
            .call(serde_json::json!({}), &mut ctx)
            .await
            .unwrap();
        assert_eq!(ctx.mode, PermissionMode::Default);
    }
}
