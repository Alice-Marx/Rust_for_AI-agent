use std::{collections::HashMap, sync::Arc};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use tokio::sync::RwLock;
use tracing::info_span;

#[async_trait]
pub trait AgentWorker: Send + Sync {
    fn name(&self) -> &str;
    async fn handle(&self, task: &str) -> Result<String>;
}

#[derive(Clone, Default)]
pub struct AgentDirectory {
    workers: Arc<RwLock<HashMap<String, Arc<dyn AgentWorker>>>>,
}

impl AgentDirectory {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn register(&self, worker: Arc<dyn AgentWorker>) {
        self.workers
            .write()
            .await
            .insert(worker.name().to_string(), worker);
    }

    pub async fn names(&self) -> Vec<String> {
        let mut names: Vec<_> = self.workers.read().await.keys().cloned().collect();
        names.sort();
        names
    }

    pub async fn call(&self, agent_name: &str, task: &str) -> Result<String> {
        let worker = self
            .workers
            .read()
            .await
            .get(agent_name)
            .cloned()
            .ok_or_else(|| anyhow!("agent '{agent_name}' is not registered"))?;
        let span = info_span!("agent_delegate", agent = agent_name, task = task);
        let _entered = span.enter();
        worker.handle(task).await
    }
}

#[derive(Debug, Default)]
pub struct ResearchAgent;

#[async_trait]
impl AgentWorker for ResearchAgent {
    fn name(&self) -> &str {
        "research"
    }

    async fn handle(&self, task: &str) -> Result<String> {
        Ok(format!("研究 Agent 已完成资料整理：{task}"))
    }
}

#[derive(Debug, Default)]
pub struct ExpenseAgent;

#[async_trait]
impl AgentWorker for ExpenseAgent {
    fn name(&self) -> &str {
        "expense"
    }

    async fn handle(&self, task: &str) -> Result<String> {
        Ok(format!("费用 Agent 已完成账目分析：{task}"))
    }
}

#[derive(Debug)]
pub struct EchoAgent {
    pub agent_name: String,
}

#[async_trait]
impl AgentWorker for EchoAgent {
    fn name(&self) -> &str {
        &self.agent_name
    }

    async fn handle(&self, task: &str) -> Result<String> {
        Ok(format!("{}: {task}", self.agent_name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn agents_can_call_each_other_through_directory() {
        let directory = AgentDirectory::new();
        directory
            .register(Arc::new(EchoAgent {
                agent_name: "helper".to_string(),
            }))
            .await;
        let result = directory.call("helper", "整理这段任务").await.unwrap();
        assert_eq!(result, "helper: 整理这段任务");
    }
}
