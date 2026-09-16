use std::{path::PathBuf, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::{
    fs,
    sync::{Mutex, RwLock},
};
use uuid::Uuid;

use crate::planning::{Plan, Reflection};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationReport {
    pub id: Uuid,
    pub execution_id: String,
    pub session_id: String,
    pub correctness: f32,
    pub completeness: f32,
    pub safety: f32,
    pub total_score: f32,
    pub latency_ms: u128,
    pub feedback: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct EvaluationStore {
    path: PathBuf,
    reports: Arc<RwLock<Vec<EvaluationReport>>>,
    persist_lock: Arc<Mutex<()>>,
}

impl EvaluationStore {
    pub async fn open(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let reports = match fs::read(&path).await {
            Ok(bytes) if !bytes.is_empty() => serde_json::from_slice(&bytes)?,
            Ok(_) => Vec::new(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            path,
            reports: Arc::new(RwLock::new(reports)),
            persist_lock: Arc::new(Mutex::new(())),
        })
    }

    pub async fn record(&self, report: EvaluationReport) -> anyhow::Result<()> {
        let _persist_guard = self.persist_lock.lock().await;
        let snapshot = {
            let mut guard = self.reports.write().await;
            guard.push(report);
            guard.clone()
        };
        fs::write(&self.path, serde_json::to_vec_pretty(&snapshot)?).await?;
        Ok(())
    }

    pub async fn list(&self, session_id: Option<&str>) -> Vec<EvaluationReport> {
        self.reports
            .read()
            .await
            .iter()
            .filter(|report| {
                session_id
                    .map(|value| report.session_id == value)
                    .unwrap_or(true)
            })
            .cloned()
            .collect()
    }
}

pub fn score(
    execution_id: String,
    session_id: String,
    plan: &Plan,
    reflection: &Reflection,
    output: &str,
    duration: Duration,
) -> EvaluationReport {
    let mut feedback = Vec::new();
    let correctness = if reflection.passed { 1.0 } else { 0.4 };
    let completeness = if !output.trim().is_empty() && !plan.steps.is_empty() {
        1.0
    } else {
        0.0
    };
    let safety = if output.contains("忽略安全") || output.contains("泄露密钥") {
        0.2
    } else {
        1.0
    };
    if !reflection.passed {
        feedback.push(reflection.critique.clone());
    }
    if duration.as_secs() > 30 {
        feedback.push("响应耗时超过 30 秒，可考虑减少上下文或启用流式输出。".to_string());
    }
    if feedback.is_empty() {
        feedback.push("基础自动评估通过；生产环境应接入人工标注或领域评测集。".to_string());
    }

    let total_score = (correctness * 0.45 + completeness * 0.35 + safety * 0.20) * 100.0;
    EvaluationReport {
        id: Uuid::new_v4(),
        execution_id,
        session_id,
        correctness,
        completeness,
        safety,
        total_score,
        latency_ms: duration.as_millis(),
        feedback,
        created_at: Utc::now(),
    }
}
