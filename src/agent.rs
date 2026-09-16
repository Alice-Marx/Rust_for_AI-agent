use std::{sync::Arc, time::Instant};

use anyhow::Result;
use tracing::{info, info_span, instrument};
use uuid::Uuid;

use crate::{
    collaboration::{AgentDirectory, ExpenseAgent, ResearchAgent},
    evaluation::{score, EvaluationStore},
    memory::{MemoryKind, MemoryStore},
    model::{AgentRequest, AgentResponse, DelegatedResult},
    planning::{HeuristicPlanner, Plan, Planner, StepStatus},
    provider::{ModelProvider, ModelRequest},
    sandbox::SandboxExecutor,
};

#[derive(Clone)]
pub struct AgentRuntime {
    pub provider: Arc<dyn ModelProvider>,
    pub memory: MemoryStore,
    pub directory: AgentDirectory,
    pub planner: Arc<dyn Planner>,
    pub sandbox: SandboxExecutor,
    pub evaluations: EvaluationStore,
    pub max_steps: usize,
}

impl AgentRuntime {
    pub fn new(
        provider: Arc<dyn ModelProvider>,
        memory: MemoryStore,
        evaluations: EvaluationStore,
        sandbox: SandboxExecutor,
    ) -> Self {
        Self {
            provider,
            memory,
            directory: AgentDirectory::new(),
            planner: Arc::new(HeuristicPlanner),
            sandbox,
            evaluations,
            max_steps: 6,
        }
    }

    pub async fn register_default_agents(&self) {
        self.directory.register(Arc::new(ResearchAgent)).await;
        self.directory.register(Arc::new(ExpenseAgent)).await;
    }

    #[instrument(skip(self, request), fields(session_id = %request.session_id))]
    pub async fn run(&self, request: AgentRequest) -> Result<AgentResponse> {
        let started = Instant::now();
        let execution_id = Uuid::new_v4().to_string();
        let span = info_span!("agent_execution", execution_id = %execution_id, session_id = %request.session_id);
        let _entered = span.enter();

        let memories = self
            .memory
            .search(request.user_id.as_deref(), &request.input, 5)
            .await;
        info!(memory_hits = memories.len(), "retrieved long-term memory");

        let mut plan = self.planner.plan(&request.input).await?;
        if plan.steps.len() > self.max_steps {
            plan.steps.truncate(self.max_steps);
        }

        let delegated = self.delegate_if_needed(&request.input).await;
        let memory_context = memories
            .iter()
            .map(|memory| format!("- {}", memory.entry.content))
            .collect::<Vec<_>>()
            .join("\n");
        let delegated_context = delegated
            .iter()
            .map(|result| format!("- {}: {}", result.agent, result.output))
            .collect::<Vec<_>>()
            .join("\n");
        let plan_json = serde_json::to_string(&plan)?;
        let system_prompt = "你是一个可靠的 Rust AI Agent。请基于计划和上下文回答用户，不要编造事实，不要泄露内部推理或密钥；需要执行高风险操作时先说明风险。";
        let user_prompt = format!(
            "用户请求：{}\n\n计划：{}\n\n相关长期记忆：{}\n\n协作 Agent 结果：{}",
            request.input,
            plan_json,
            if memory_context.is_empty() {
                "无"
            } else {
                &memory_context
            },
            if delegated_context.is_empty() {
                "无"
            } else {
                &delegated_context
            },
        );

        let mut output = self
            .provider
            .complete(ModelRequest {
                system_prompt: system_prompt.to_string(),
                user_prompt,
            })
            .await?
            .text;
        mark_completed(&mut plan);
        let mut reflection = self.planner.reflect(&plan, &output).await?;

        if !reflection.passed && reflection.retry {
            let correction_prompt = format!(
                "上一次回答未通过自检：{}。请重新回答用户请求，并给出可执行、简洁的结论。用户请求：{}",
                reflection.critique, request.input
            );
            output = self
                .provider
                .complete(ModelRequest {
                    system_prompt: system_prompt.to_string(),
                    user_prompt: correction_prompt,
                })
                .await?
                .text;
            reflection = self.planner.reflect(&plan, &output).await?;
        }

        self.memory
            .remember(
                request.user_id.clone(),
                Some(request.session_id.clone()),
                format!("用户：{}\nAgent：{}", request.input, output),
                MemoryKind::Conversation,
                vec!["agent-run".to_string()],
                0.35,
            )
            .await?;

        let evaluation = score(
            execution_id.clone(),
            request.session_id.clone(),
            &plan,
            &reflection,
            &output,
            started.elapsed(),
        );
        self.evaluations.record(evaluation.clone()).await?;

        Ok(AgentResponse {
            execution_id,
            session_id: request.session_id,
            output,
            plan,
            reflection,
            delegated,
            memories,
            evaluation,
        })
    }

    async fn delegate_if_needed(&self, input: &str) -> Vec<DelegatedResult> {
        let mut requests = Vec::new();
        let lower = input.to_lowercase();
        if ["研究", "搜索", "查找", "research", "latest"]
            .iter()
            .any(|word| lower.contains(word))
        {
            requests.push(("research", "整理与用户请求相关的研究线索"));
        }
        if ["费用", "账单", "支出", "expense", "budget"]
            .iter()
            .any(|word| lower.contains(word))
        {
            requests.push(("expense", "分析与用户请求相关的费用信息"));
        }

        let mut results = Vec::new();
        for (agent, task) in requests {
            match self.directory.call(agent, task).await {
                Ok(output) => results.push(DelegatedResult {
                    agent: agent.to_string(),
                    task: task.to_string(),
                    output,
                }),
                Err(error) => tracing::warn!(agent, error = %error, "delegated agent failed"),
            }
        }
        results
    }
}

fn mark_completed(plan: &mut Plan) {
    for step in &mut plan.steps {
        step.status = if matches!(step.status, StepStatus::Revised) {
            StepStatus::Revised
        } else {
            StepStatus::Completed
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        evaluation::EvaluationStore, model::AgentRequest, provider::RuleBasedModel,
        sandbox::SandboxPolicy,
    };

    #[tokio::test]
    async fn runtime_persists_memory_and_evaluation() {
        let directory = tempfile::tempdir().unwrap();
        let memory = MemoryStore::open(directory.path().join("memory.json"))
            .await
            .unwrap();
        let evaluations = EvaluationStore::open(directory.path().join("evaluations.json"))
            .await
            .unwrap();
        let runtime = AgentRuntime::new(
            Arc::new(RuleBasedModel),
            memory.clone(),
            evaluations.clone(),
            SandboxExecutor::new(SandboxPolicy::default()),
        );
        runtime.register_default_agents().await;
        let response = runtime
            .run(AgentRequest {
                session_id: "s1".to_string(),
                user_id: Some("u1".to_string()),
                input: "请研究 Rust 的费用预算".to_string(),
            })
            .await
            .unwrap();
        assert!(!response.output.is_empty());
        assert_eq!(response.delegated.len(), 2);
        assert_eq!(memory.all().await.len(), 1);
        assert_eq!(evaluations.list(Some("s1")).await.len(), 1);
    }
}
