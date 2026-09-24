use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::Result;
use wonderland::{
    agent::AgentRuntime,
    api::{router, AppState},
    cliproxy::CliProxyApiClient,
    evaluation::EvaluationStore,
    expenses::ExpenseStore,
    memory::MemoryStore,
    observability::init_tracing,
    provider::provider_from_env_async,
    sandbox::{SandboxExecutor, SandboxPolicy},
    session::SessionStore,
    session_index::SessionIndex,
    skills::{default_skill_directories, SkillCatalog},
    tools::ToolRegistry,
};

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing()?;

    // Discover the user's real npm global prefix (it is frequently moved off
    // the C drive) so installed CLI tools are found by probes and executors.
    wonderland::app_installer::ensure_npm_prefix_on_path().await;

    let data_dir = PathBuf::from(
        std::env::var("AGENT_DATA_DIR").unwrap_or_else(|_| ".agent-data".to_string()),
    );
    let memory = MemoryStore::open(data_dir.join("memory.json")).await?;
    let evaluations = EvaluationStore::open(data_dir.join("evaluations.json")).await?;
    let skills = SkillCatalog::open(default_skill_directories(&data_dir)).await?;
    let provider = match wonderland::connection::ConnectionSettings::load(&data_dir)? {
        Some(settings) => settings.build().await?,
        None => provider_from_env_async().await?,
    };
    // 管理客户端：优先环境变量；订阅模式下自动指向刚拉起的 sidecar，
    // 因此只登录订阅账号即可使用，无需手工配置 CLIPROXYAPI_*。
    let cliproxy = CliProxyApiClient::from_env()
        .or_else(|| {
            wonderland::provider::active_subscription_endpoint().map(|endpoint| {
                CliProxyApiClient::new(
                    endpoint.base_url.clone(),
                    Some(endpoint.api_key.clone()),
                    endpoint.management_url.clone(),
                    Some(endpoint.management_key.clone()),
                )
            })
        })
        .map(Arc::new);
    let sandbox = SandboxExecutor::new(SandboxPolicy {
        enabled: env_bool("AGENT_ENABLE_SANDBOX", true),
        memory_limit_mb: Some(env_u64("AGENT_SANDBOX_MEMORY_MB", 512)),
        max_processes: Some(env_u64("AGENT_SANDBOX_MAX_PROCESSES", 8).clamp(1, 128) as u32),
        max_output_bytes: env_u64("AGENT_SANDBOX_MAX_OUTPUT_BYTES", 65_536).clamp(1024, 16_777_216)
            as usize,
        timeout_ms: env_u64("AGENT_SANDBOX_TIMEOUT_MS", 2_000),
        ..SandboxPolicy::default()
    });

    // 会话检索索引：派生数据，启动时按目录重建一次，之后随每轮落盘增量更新。
    let sessions = SessionStore::new(data_dir.join("sessions"));
    let index = match SessionIndex::open(data_dir.join("sessions-index.sqlite")) {
        Ok(index) => {
            match index.rebuild(&sessions) {
                Ok(count) => tracing::info!(sessions = count, "session index rebuilt"),
                Err(error) => tracing::warn!(%error, "session index rebuild failed"),
            }
            Some(Arc::new(index))
        }
        Err(error) => {
            tracing::warn!(%error, "session index unavailable; falling back to linear scan");
            None
        }
    };

    let runtime = {
        let mut runtime = AgentRuntime::new(
            provider,
            memory,
            evaluations,
            sandbox,
            ToolRegistry::builtin(),
            sessions,
        )
        .with_skills(skills);
        if let Some(index) = index.clone() {
            runtime = runtime.with_index(index);
        }
        runtime.register_default_agents().await;
        // Task 工具需要 directory 才能委派子代理。
        let mut tools = ToolRegistry::builtin_with_directory(runtime.directory.clone());
        // MCP：把 .mcp.json / .claude/settings.json / .wonderland/settings.json
        // 里配置的服务器工具（mcp__<server>__<tool>）注册进同一张工具表。
        if runtime.sandbox.policy().enabled {
            tools.register(Arc::new(wonderland::tools::sandbox::SandboxRun(
                runtime.sandbox.clone(),
            )));
        }
        let cwd = std::env::current_dir()?;
        let mcp = wonderland::mcp::load_tools(&cwd).await;
        *runtime.mcp_status.write().await = wonderland::mcp::summaries(&mcp);
        for (server, tool_count) in &mcp.servers {
            tracing::info!(server = %server, tools = tool_count, "connected mcp server");
        }
        for (server, error) in &mcp.errors {
            tracing::warn!(server = %server, %error, "mcp server unavailable");
        }
        for tool in mcp.tools {
            tools.register(tool);
        }
        runtime.tools = tools;
        runtime
    };
    let runtime = Arc::new(runtime);

    let address: SocketAddr = std::env::var("AGENT_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8080".to_string())
        .parse()?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    anyhow::ensure!(
        address.ip().is_loopback()
            || std::env::var("WONDERLAND_SERVER_TOKEN").is_ok_and(|t| !t.is_empty()),
        "binding outside loopback requires WONDERLAND_SERVER_TOKEN"
    );
    tracing::info!(%address, "Wonderland API started");
    let workbench = wonderland::workbench_service::WorkbenchService::open(&data_dir)?;
    let shutdown_workbench = workbench.clone();
    axum::serve(
        listener,
        router(AppState {
            runtime,
            expenses: ExpenseStore::seeded(),
            cliproxy,
            workbench,
        }),
    )
    .with_graceful_shutdown(async move {
        let _ = tokio::signal::ctrl_c().await;
        shutdown_workbench.shutdown().await;
    })
    .await?;
    wonderland::provider::stop_subscription().await;
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
