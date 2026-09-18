use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use futures_util::stream::Stream;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use tower_http::trace::TraceLayer;

use crate::{
    agent::AgentRuntime,
    cliproxy::CliProxyApiClient,
    expenses::ExpenseStore,
    memory::MemoryKind,
    model::{AgentRequest, MemoryWriteRequest, SandboxRequest},
    skills::SkillSummary,
};

#[derive(Clone)]
pub struct AppState {
    pub runtime: Arc<AgentRuntime>,
    pub expenses: ExpenseStore,
    pub cliproxy: Option<Arc<CliProxyApiClient>>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/agent/run", post(run_agent))
        .route("/v1/agent/stream", post(run_agent_stream))
        .route("/v1/memory", post(write_memory))
        .route("/v1/memory/search", get(search_memory))
        .route("/v1/sandbox/execute", post(execute_sandbox))
        .route("/v1/evaluations", get(list_evaluations))
        .route("/v1/sessions", get(list_sessions))
        .route("/v1/sessions/search", get(search_sessions))
        .route("/v1/sessions/{id}", get(get_session))
        .route("/v1/skills", get(list_skills))
        .route("/v1/tools", get(list_tools))
        .route("/v1/mcp/servers", get(list_mcp_servers))
        .route("/v1/skills/reload", post(reload_skills))
        .route(
            "/v1/providers/cliproxyapi/models",
            get(list_cliproxy_models),
        )
        .route(
            "/v1/providers/cliproxyapi/accounts",
            get(list_cliproxy_accounts),
        )
        .route(
            "/v1/providers/cliproxyapi/accounts/refresh",
            post(refresh_cliproxy_accounts),
        )
        .route("/v1/providers/cliproxyapi/verify", post(verify_cliproxy))
        .route(
            "/v1/providers/cliproxyapi/login",
            post(start_cliproxy_login).delete(cancel_cliproxy_login),
        )
        .route(
            "/v1/providers/cliproxyapi/login/status",
            get(cliproxy_login_status),
        )
        .nest("/expenses", expense_router())
        .with_state(state)
        .layer(TraceLayer::new_for_http())
}

fn expense_router() -> Router<AppState> {
    Router::new()
        .route(
            "/",
            get(crate::expenses::list).post(crate::expenses::create),
        )
        .route(
            "/{id}",
            get(crate::expenses::get)
                .put(crate::expenses::update)
                .delete(crate::expenses::delete),
        )
        .route("/summary", get(crate::expenses::summary))
        .layer(middleware::from_fn(require_expense_api_key))
}

async fn require_expense_api_key(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<Response, StatusCode> {
    let expected =
        std::env::var("EXPENSE_API_KEY").unwrap_or_else(|_| "dev-secret-key".to_string());
    let provided = request
        .headers()
        .get("x-api-key")
        .and_then(|value| value.to_str().ok());
    match provided {
        Some(value) if value == expected => Ok(next.run(request).await),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

/// SSE 帧：先流式推送增量事件，最后补一帧完整响应（或错误）。
#[derive(Serialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
enum SseFrame {
    Event {
        event: crate::provider::StreamEvent,
    },
    Response {
        response: Box<crate::model::AgentResponse>,
    },
    Error {
        message: String,
    },
}

/// 流式运行 Agent：以 SSE 推送增量事件，最后一条响应帧带上完整结果。
///
/// 事件形状见 provider::StreamEvent，例如
/// data: {"type":"text_delta","text":"..."}
async fn run_agent_stream(
    State(state): State<AppState>,
    Json(request): Json<AgentRequest>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let (sink, receiver) = tokio::sync::mpsc::unbounded_channel::<crate::provider::StreamEvent>();
    let (frames, frame_receiver) = tokio::sync::mpsc::unbounded_channel::<SseFrame>();
    let runtime = state.runtime.clone();
    tokio::spawn(async move {
        // 事件先转发为 frame，run 结束后补一帧完整响应（含 plan/todos/usage）。
        let forward_frames = frames.clone();
        let forward = tokio::spawn(async move {
            let mut receiver = receiver;
            while let Some(event) = receiver.recv().await {
                if forward_frames.send(SseFrame::Event { event }).is_err() {
                    break;
                }
            }
        });
        let outcome = runtime
            .run_with_events(
                request,
                Arc::new(crate::permissions::DenyAllHandler),
                Some(sink),
            )
            .await;
        let _ = forward.await;
        match outcome {
            Ok(response) => {
                let _ = frames.send(SseFrame::Response {
                    response: Box::new(response),
                });
            }
            Err(error) => {
                let _ = frames.send(SseFrame::Error {
                    message: error.to_string(),
                });
            }
        }
    });

    let stream = futures_util::stream::unfold(frame_receiver, |mut receiver| async move {
        let frame = receiver.recv().await?;
        let payload = serde_json::to_string(&frame).unwrap_or_else(|_| "{}".to_string());
        Some((Ok(Event::default().data(payload)), receiver))
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    agents: Vec<String>,
    sandbox_enabled: bool,
    /// 沙箱策略与实际隔离机制。
    sandbox: serde_json::Value,
    /// 当前 provider 可用的 wire 协议（chat / responses / anthropic）。
    protocols: Vec<String>,
    provider: &'static str,
    cliproxyapi_configured: bool,
    skills: usize,
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        agents: state.runtime.directory.names().await,
        sandbox_enabled: state.runtime.sandbox.policy().enabled,
        sandbox: state.runtime.sandbox.describe(),
        protocols: state
            .runtime
            .provider
            .available_protocols()
            .into_iter()
            .map(str::to_string)
            .collect(),
        provider: state.runtime.provider.name(),
        cliproxyapi_configured: state.cliproxy.is_some(),
        skills: state.runtime.skills.summaries().await.len(),
    })
}

#[derive(Debug, Deserialize)]
pub struct CliProxyVerifyRequest {
    pub model: Option<String>,
}

/// 已登录的订阅账号。
async fn list_cliproxy_accounts(
    State(state): State<AppState>,
) -> Result<Json<Vec<crate::cliproxy::CliProxyAccount>>, ApiError> {
    let client = state
        .cliproxy
        .clone()
        .ok_or_else(|| ApiError::service_unavailable("CLIProxyAPI is not configured"))?;
    Ok(Json(client.list_accounts().await?))
}

#[derive(Serialize)]
struct RefreshAccountsResponse {
    refreshed: bool,
}

async fn refresh_cliproxy_accounts(
    State(state): State<AppState>,
) -> Result<Json<RefreshAccountsResponse>, ApiError> {
    let client = state
        .cliproxy
        .clone()
        .ok_or_else(|| ApiError::service_unavailable("CLIProxyAPI is not configured"))?;
    Ok(Json(RefreshAccountsResponse {
        refreshed: client.refresh_accounts().await?,
    }))
}

async fn list_cliproxy_models(
    State(state): State<AppState>,
) -> Result<Json<Vec<crate::cliproxy::CliProxyModel>>, ApiError> {
    let client = state
        .cliproxy
        .ok_or_else(|| ApiError::service_unavailable(cliproxy_not_ready_message()))?;
    Ok(Json(client.list_models().await?))
}

async fn verify_cliproxy(
    State(state): State<AppState>,
    Json(request): Json<CliProxyVerifyRequest>,
) -> Result<Json<crate::cliproxy::CliProxyVerification>, ApiError> {
    let client = state
        .cliproxy
        .ok_or_else(|| ApiError::service_unavailable(cliproxy_not_ready_message()))?;
    Ok(Json(client.verify(request.model).await?))
}

#[derive(Debug, Deserialize)]
pub struct CliProxyLoginRequest {
    pub provider: String,
}

async fn start_cliproxy_login(
    State(state): State<AppState>,
    Json(request): Json<CliProxyLoginRequest>,
) -> Result<Json<crate::cliproxy::CliProxyLoginStart>, ApiError> {
    let client = state
        .cliproxy
        .ok_or_else(|| ApiError::service_unavailable(cliproxy_not_ready_message()))?;
    Ok(Json(client.start_login(&request.provider).await?))
}

#[derive(Debug, Deserialize)]
pub struct CliProxyLoginQuery {
    pub state: String,
}

async fn cliproxy_login_status(
    State(state): State<AppState>,
    Query(query): Query<CliProxyLoginQuery>,
) -> Result<Json<crate::cliproxy::CliProxyLoginStatus>, ApiError> {
    let client = state
        .cliproxy
        .ok_or_else(|| ApiError::service_unavailable(cliproxy_not_ready_message()))?;
    Ok(Json(client.login_status(&query.state).await?))
}

async fn cancel_cliproxy_login(
    State(state): State<AppState>,
    Query(query): Query<CliProxyLoginQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let client = state
        .cliproxy
        .ok_or_else(|| ApiError::service_unavailable(cliproxy_not_ready_message()))?;
    let cancelled = client.cancel_login(&query.state).await?;
    Ok(Json(serde_json::json!({
        "status": "ok",
        "cancelled": cancelled
    })))
}

fn cliproxy_not_ready_message() -> &'static str {
    "CLIProxyAPI is not configured. Start the packaged desktop launcher, or configure AGENT_PROVIDER=cliproxyapi and CLIPROXYAPI_* before starting the Rust Agent service."
}

async fn run_agent(
    State(state): State<AppState>,
    Json(request): Json<AgentRequest>,
) -> Result<Json<crate::model::AgentResponse>, ApiError> {
    Ok(Json(state.runtime.run(request).await?))
}

async fn write_memory(
    State(state): State<AppState>,
    Json(request): Json<MemoryWriteRequest>,
) -> Result<Json<crate::memory::MemoryEntry>, ApiError> {
    let entry = state
        .runtime
        .memory
        .remember(
            request.user_id,
            request.session_id,
            request.content,
            MemoryKind::Fact,
            request.tags,
            request.importance,
        )
        .await?;
    Ok(Json(entry))
}

#[derive(Debug, Deserialize)]
struct MemorySearchQuery {
    q: String,
    user_id: Option<String>,
    limit: Option<usize>,
}

async fn search_memory(
    State(state): State<AppState>,
    Query(query): Query<MemorySearchQuery>,
) -> Json<Vec<crate::memory::MemoryMatch>> {
    Json(
        state
            .runtime
            .memory
            .search(
                query.user_id.as_deref(),
                &query.q,
                query.limit.unwrap_or(10).min(50),
            )
            .await,
    )
}

async fn execute_sandbox(
    State(state): State<AppState>,
    Json(request): Json<SandboxRequest>,
) -> Result<Json<crate::sandbox::SandboxResult>, ApiError> {
    Ok(Json(state.runtime.sandbox.execute(request).await?))
}

#[derive(Debug, Deserialize)]
struct EvaluationQuery {
    session_id: Option<String>,
}

async fn list_evaluations(
    State(state): State<AppState>,
    Query(query): Query<EvaluationQuery>,
) -> Json<Vec<crate::evaluation::EvaluationReport>> {
    Json(
        state
            .runtime
            .evaluations
            .list(query.session_id.as_deref())
            .await,
    )
}

#[derive(Debug, Deserialize)]
struct SessionQuery {
    user_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SessionSearchQuery {
    q: String,
    user_id: Option<String>,
    limit: Option<usize>,
}

/// 会话关键词检索（SQLite 索引）。索引未启用时回退到线性扫描。
async fn search_sessions(
    State(state): State<AppState>,
    Query(query): Query<SessionSearchQuery>,
) -> Result<Json<Vec<crate::session_index::IndexedSession>>, ApiError> {
    let limit = query.limit.unwrap_or(20).clamp(1, 200);
    if let Some(index) = &state.runtime.index {
        return Ok(Json(index.search(
            &query.q,
            query.user_id.as_deref(),
            limit,
        )?));
    }
    Ok(Json(scan_sessions(
        &state,
        &query.q,
        query.user_id.as_deref(),
        limit,
    )?))
}

/// 索引不可用时的兜底：直接扫会话文件。
fn scan_sessions(
    state: &AppState,
    query: &str,
    user_id: Option<&str>,
    limit: usize,
) -> anyhow::Result<Vec<crate::session_index::IndexedSession>> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Ok(Vec::new());
    }
    let mut hits = Vec::new();
    for summary in state.runtime.sessions.list()? {
        if let Some(user_id) = user_id {
            if summary.user_id.as_deref() != Some(user_id) {
                continue;
            }
        }
        let Some(session) = state.runtime.sessions.load(&summary.id)? else {
            continue;
        };
        let rendered = session
            .messages
            .iter()
            .map(|message| message.text())
            .collect::<Vec<_>>()
            .join(" ");
        if let Some(position) = rendered.to_lowercase().find(&needle) {
            let chars: Vec<char> = rendered.chars().collect();
            let start = position.saturating_sub(60);
            let end = (start + 180).min(chars.len());
            hits.push(crate::session_index::IndexedSession {
                id: session.id.clone(),
                user_id: session.user_id.clone(),
                updated_at: session.updated_at.to_rfc3339(),
                message_count: session.messages.len(),
                snippet: chars[start..end].iter().collect(),
            });
        }
        if hits.len() >= limit {
            break;
        }
    }
    Ok(hits)
}

async fn list_sessions(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<Vec<crate::session::SessionSummary>>, ApiError> {
    let sessions = state.runtime.sessions.list()?;
    let sessions = match query.user_id.as_deref().filter(|value| !value.is_empty()) {
        Some(user_id) => sessions
            .into_iter()
            .filter(|session| session.user_id.as_deref() == Some(user_id))
            .collect(),
        None => sessions,
    };
    Ok(Json(sessions))
}

async fn get_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<crate::session::Session>, ApiError> {
    let session = state
        .runtime
        .sessions
        .load(&id)?
        .ok_or_else(|| ApiError::not_found(format!("session not found: {id}")))?;
    Ok(Json(session))
}

async fn list_skills(State(state): State<AppState>) -> Json<Vec<SkillSummary>> {
    Json(state.runtime.skills.summaries().await)
}

#[derive(Serialize)]
struct ToolSummary {
    name: String,
    read_only: bool,
    always_allowed: bool,
    /// 内置工具为 `builtin`，MCP 工具为 `mcp__<server>` 前缀对应的服务器名。
    source: String,
}

/// 当前注册的全部工具（内置 + MCP），便于前端展示与排障。
async fn list_tools(State(state): State<AppState>) -> Json<Vec<ToolSummary>> {
    Json(
        state
            .runtime
            .tools
            .iter()
            .map(|tool| ToolSummary {
                name: tool.name().to_string(),
                read_only: tool.is_read_only(&serde_json::json!({})),
                always_allowed: tool.is_always_allowed(),
                source: crate::mcp::tool_source(tool.name()),
            })
            .collect(),
    )
}

#[derive(Serialize)]
struct McpServerSummary {
    name: String,
    tools: Vec<String>,
}

/// 已连接的 MCP 服务器及其暴露的工具名。
async fn list_mcp_servers(State(state): State<AppState>) -> Json<Vec<McpServerSummary>> {
    let mut servers: Vec<McpServerSummary> = Vec::new();
    for tool in state.runtime.tools.iter() {
        let Some(server) = crate::mcp::tool_server(tool.name()) else {
            continue;
        };
        match servers.iter_mut().find(|entry| entry.name == server) {
            Some(entry) => entry.tools.push(tool.name().to_string()),
            None => servers.push(McpServerSummary {
                name: server.to_string(),
                tools: vec![tool.name().to_string()],
            }),
        }
    }
    servers.sort_by(|left, right| left.name.cmp(&right.name));
    Json(servers)
}

#[derive(Serialize)]
struct ReloadSkillsResponse {
    status: &'static str,
    skills: usize,
}

async fn reload_skills(
    State(state): State<AppState>,
) -> Result<Json<ReloadSkillsResponse>, ApiError> {
    let skills = state.runtime.skills.reload().await?;
    Ok(Json(ReloadSkillsResponse {
        status: "ok",
        skills,
    }))
}

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    error: anyhow::Error,
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            error,
        }
    }
}

impl ApiError {
    fn service_unavailable(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            error: anyhow::anyhow!(message.into()),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            error: anyhow::anyhow!(message.into()),
        }
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: self.error.to_string(),
            }),
        )
            .into_response()
    }
}
