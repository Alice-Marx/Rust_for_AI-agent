use serde::{Deserialize, Serialize};

use crate::{evaluation::EvaluationReport, memory::MemoryMatch, planning::{Plan, Reflection}};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRequest {
    pub session_id: String,
    #[serde(default)]
    pub user_id: Option<String>,
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
    pub memories: Vec<MemoryMatch>,
    pub evaluation: EvaluationReport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegatedResult {
    pub agent: String,
    pub task: String,
    pub output: String,
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
