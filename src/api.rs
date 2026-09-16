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
    expenses::ExpenseStore,
    memory::MemoryKind,
    model::{AgentRequest, MemoryWriteRequest, SandboxRequest},
};

#[derive(Clone)]
pub struct AppState {
    pub runtime: Arc<AgentRuntime>,
    pub expenses: ExpenseStore,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/agent/run", post(run_agent))
        .route("/v1/memory", post(write_memory))
        .route("/v1/memory/search", get(search_memory))
        .route("/v1/sandbox/execute", post(execute_sandbox))
        .route("/v1/evaluations", get(list_evaluations))
        .nest("/expenses", expense_router())
        .with_state(state)
        .layer(TraceLayer::new_for_http())
}

fn expense_router() -> Router<AppState> {
    Router::new()
        .route("/", get(crate::expenses::list).post(crate::expenses::create))
        .route("/{id}", get(crate::expenses::get).put(crate::expenses::update).delete(crate::expenses::delete))
        .route("/summary", get(crate::expenses::summary))
        .layer(middleware::from_fn(require_expense_api_key))
}

async fn require_expense_api_key(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<Response, StatusCode> {
    let expected = std::env::var("EXPENSE_API_KEY").unwrap_or_else(|_| "dev-secret-key".to_string());
    let provided = request.headers().get("x-api-key").and_then(|value| value.to_str().ok());
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
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        agents: state.runtime.directory.names().await,
        sandbox_enabled: state.runtime.sandbox.policy().enabled,
    })
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
            .search(query.user_id.as_deref(), &query.q, query.limit.unwrap_or(10).min(50))
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
    Json(state.runtime.evaluations.list(query.session_id.as_deref()).await)
}

#[derive(Debug)]
pub struct ApiError(anyhow::Error);

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        Self(error)
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorBody { error: self.0.to_string() }),
        )
            .into_response()
    }
}
