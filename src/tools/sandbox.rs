use super::{Tool, ToolContext, ToolOutput};
use crate::{model::SandboxRequest, sandbox::SandboxExecutor};
use serde_json::{json, Value};

pub struct SandboxRun(pub SandboxExecutor);
#[async_trait::async_trait]
impl Tool for SandboxRun {
    fn name(&self) -> &str {
        "SandboxRun"
    }
    fn description(&self) -> &str {
        "Run Python or Node code in an OS-isolated disposable directory. No project files, credentials or network are available. Use for untrusted calculations and code experiments. Bash and file tools operate on the host separately."
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{"language":{"type":"string","enum":["python","node"]},"code":{"type":"string"},"timeout_ms":{"type":"integer","minimum":1}},"required":["language","code"],"additionalProperties":false})
    }
    async fn call(&self, input: Value, _ctx: &mut ToolContext) -> anyhow::Result<ToolOutput> {
        let result = self
            .0
            .execute(serde_json::from_value::<SandboxRequest>(input)?)
            .await?;
        let failed = result.timed_out || result.exit_code != Some(0);
        let content = serde_json::to_string(&result)?;
        Ok(if failed {
            ToolOutput::err(content)
        } else {
            ToolOutput::ok(content)
        })
    }
}
