use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::Result;
use rust_ai_agent::{
    agent::AgentRuntime,
    api::{router, AppState},
    cliproxy::CliProxyApiClient,
    evaluation::EvaluationStore,
    expenses::ExpenseStore,
    memory::MemoryStore,
    observability::init_tracing,
    provider::provider_from_env,
    sandbox::{SandboxExecutor, SandboxPolicy},
};

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing()?;

    let data_dir = PathBuf::from(
        std::env::var("AGENT_DATA_DIR").unwrap_or_else(|_| ".agent-data".to_string()),
    );
    let memory = MemoryStore::open(data_dir.join("memory.json")).await?;
    let evaluations = EvaluationStore::open(data_dir.join("evaluations.json")).await?;
    let provider = provider_from_env()?;
    let cliproxy = CliProxyApiClient::from_env().map(Arc::new);
    let sandbox = SandboxExecutor::new(SandboxPolicy {
        enabled: env_bool("AGENT_ENABLE_SANDBOX", false),
        timeout_ms: env_u64("AGENT_SANDBOX_TIMEOUT_MS", 2_000),
        ..SandboxPolicy::default()
    });

    let runtime = Arc::new(AgentRuntime::new(provider, memory, evaluations, sandbox));
    runtime.register_default_agents().await;

    let address: SocketAddr = std::env::var("AGENT_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8080".to_string())
        .parse()?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    tracing::info!(%address, "Rust AI Agent API started");
    axum::serve(
        listener,
        router(AppState {
            runtime,
            expenses: ExpenseStore::seeded(),
            cliproxy,
        }),
    )
    .await?;
    Ok(())
}

fn env_bool(name: &str, default: bool) -> bool {
    std::env::var(name)
        .ok()
        .map(|value| matches!(value.to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}
