use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tower_http::trace::TraceLayer;

use crate::{
    agent::AgentRuntime,
    cliproxy::CliProxyApiClient,
    expenses::ExpenseStore,
    memory::MemoryKind,
    model::{AgentRequest, MemoryWriteRequest, SandboxRequest},
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
        .route("/v1/memory", post(write_memory))
        .route("/v1/memory/search", get(search_memory))
        .route("/v1/sandbox/execute", post(execute_sandbox))
        .route("/v1/evaluations", get(list_evaluations))
        .route(
            "/v1/providers/cliproxyapi/models",
            get(list_cliproxy_models),
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

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    agents: Vec<String>,
    sandbox_enabled: bool,
    provider: &'static str,
    cliproxyapi_configured: bool,
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        agents: state.runtime.directory.names().await,
        sandbox_enabled: state.runtime.sandbox.policy().enabled,
        provider: state.runtime.provider.name(),
        cliproxyapi_configured: state.cliproxy.is_some(),
    })
}

#[derive(Debug, Deserialize)]
pub struct CliProxyVerifyRequest {
    pub model: Option<String>,
}

async fn list_cliproxy_models(
    State(state): State<AppState>,
) -> Result<Json<Vec<crate::cliproxy::CliProxyModel>>, ApiError> {
    let client = state
        .cliproxy
        .ok_or_else(|| ApiError::service_unavailable("CLIProxyAPI is not configured"))?;
    Ok(Json(client.list_models().await?))
}

async fn verify_cliproxy(
    State(state): State<AppState>,
    Json(request): Json<CliProxyVerifyRequest>,
) -> Result<Json<crate::cliproxy::CliProxyVerification>, ApiError> {
    let client = state
        .cliproxy
        .ok_or_else(|| ApiError::service_unavailable("CLIProxyAPI is not configured"))?;
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
        .ok_or_else(|| ApiError::service_unavailable("CLIProxyAPI is not configured"))?;
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
        .ok_or_else(|| ApiError::service_unavailable("CLIProxyAPI is not configured"))?;
    Ok(Json(client.login_status(&query.state).await?))
}

async fn cancel_cliproxy_login(
    State(state): State<AppState>,
    Query(query): Query<CliProxyLoginQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let client = state
        .cliproxy
        .ok_or_else(|| ApiError::service_unavailable("CLIProxyAPI is not configured"))?;
    let cancelled = client.cancel_login(&query.state).await?;
    Ok(Json(serde_json::json!({
        "status": "ok",
        "cancelled": cancelled
    })))
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
