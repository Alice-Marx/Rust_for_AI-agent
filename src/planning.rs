use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub goal: String,
    pub steps: Vec<PlanStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    pub id: usize,
    pub description: String,
    pub status: StepStatus,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    Completed,
    Revised,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reflection {
    pub passed: bool,
    pub critique: String,
    pub retry: bool,
}

#[async_trait]
pub trait Planner: Send + Sync {
    async fn plan(&self, goal: &str) -> Result<Plan>;
    async fn reflect(&self, plan: &Plan, output: &str) -> Result<Reflection>;
}

#[derive(Debug, Default)]
pub struct HeuristicPlanner;

#[async_trait]
impl Planner for HeuristicPlanner {
    async fn plan(&self, goal: &str) -> Result<Plan> {
        let mut pieces: Vec<String> = goal
            .split(|character: char| ".!?。！？；;".contains(character))
            .map(str::trim)
            .filter(|piece| !piece.is_empty())
            .map(ToOwned::to_owned)
            .collect();

        if pieces.is_empty() {
            pieces.push(goal.trim().to_string());
        }
        if pieces.len() == 1 {
            pieces.insert(0, "理解目标并检索相关长期记忆".to_string());
        }
        pieces.truncate(6);

        Ok(Plan {
            goal: goal.to_string(),
            steps: pieces
                .into_iter()
                .enumerate()
                .map(|(index, description)| PlanStep {
                    id: index + 1,
                    description,
                    status: StepStatus::Pending,
                })
                .collect(),
        })
    }

    async fn reflect(&self, _plan: &Plan, output: &str) -> Result<Reflection> {
        let normalized = output.trim().to_lowercase();
        let failed = normalized.is_empty()
            || normalized.contains("无法完成")
            || normalized.contains("unable to complete")
            || normalized.contains("error:");

        Ok(if failed {
            Reflection {
                passed: false,
                critique: "输出为空或明确表示失败，需要一次修正尝试。".to_string(),
                retry: true,
            }
        } else {
            Reflection {
                passed: true,
                critique: "输出非空且未发现明显失败信号。".to_string(),
                retry: false,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn planner_creates_steps_and_reflection_accepts_answer() {
        let planner = HeuristicPlanner;
        let plan = planner.plan("检索记忆。总结结果").await.unwrap();
        assert_eq!(plan.steps.len(), 2);
        let reflection = planner.reflect(&plan, "完成了总结").await.unwrap();
        assert!(reflection.passed);
    }
}
