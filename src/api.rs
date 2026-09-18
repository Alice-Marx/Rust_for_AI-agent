use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{any, get, post},
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
    provider::ModelProvider,
    skills::SkillSummary,
};

#[derive(Clone)]
pub struct AppState {
    pub runtime: Arc<AgentRuntime>,
    pub expenses: ExpenseStore,
    pub cliproxy: Option<Arc<CliProxyApiClient>>,
}

impl AppState {
    fn subscription_client(&self) -> Option<Arc<CliProxyApiClient>> {
        self.cliproxy.clone().or_else(|| {
            crate::provider::active_subscription_endpoint().map(|e| {
                Arc::new(CliProxyApiClient::new(
                    e.base_url.clone(),
                    Some(e.api_key.clone()),
                    e.management_url.clone(),
                    Some(e.management_key.clone()),
                ))
            })
        })
    }
}

async fn list_models(
    State(state): State<AppState>,
) -> Result<Json<Vec<crate::cliproxy::CliProxyModel>>, ApiError> {
    Ok(Json(state.runtime.provider.list_models().await?))
}

async fn get_connection(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let directory = state
        .runtime
        .sessions
        .dir()
        .parent()
        .unwrap_or(std::path::Path::new("."));
    Ok(Json(
        match crate::connection::ConnectionSettings::load(directory)? {
            Some(settings) => settings.public(),
            None => {
                serde_json::json!({"provider":std::env::var("AGENT_PROVIDER").unwrap_or_else(|_|state.runtime.provider.name().into()),"model":state.runtime.provider.default_model(),"base_url":"","has_api_key":false})
            }
        },
    ))
}
async fn set_connection(
    State(state): State<AppState>,
    Json(mut settings): Json<crate::connection::ConnectionSettings>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let directory = state
        .runtime
        .sessions
        .dir()
        .parent()
        .unwrap_or(std::path::Path::new("."));
    if settings.api_key.is_empty() {
        if let Some(previous) = crate::connection::ConnectionSettings::load(directory)? {
            if previous.provider == settings.provider && previous.base_url == settings.base_url {
                settings.api_key = previous.api_key;
            }
        }
    }
    let provider = settings.build().await?;
    settings.save(directory)?;
    state.runtime.provider.replace(provider);
    Ok(Json(settings.public()))
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
        .route("/v1/mcp/reload", post(reload_mcp))
        .route("/v1/mcp/{name}/login", post(mcp_login).delete(mcp_logout))
        .route("/v1/mcp/login/status", get(mcp_login_status))
        .route("/v1/models/profile", get(model_profile))
        .route("/v1/models", get(list_models))
        .route("/v1/connection", get(get_connection).put(set_connection))
        .route("/v1/permissions/{id}", post(answer_permission))
        .route(
            "/v1/providers/cliproxyapi/management/{*path}",
            any(proxy_management),
        )
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
        .layer(middleware::from_fn(protect_local_api))
}

async fn protect_local_api(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<Response, StatusCode> {
    if let Some(origin) = request
        .headers()
        .get("origin")
        .and_then(|h| h.to_str().ok())
    {
        let expected = request
            .headers()
            .get("host")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        if origin != format!("http://{expected}") && origin != format!("https://{expected}") {
            return Err(StatusCode::FORBIDDEN);
        }
    }
    if request.uri().path() != "/health" {
        if let Ok(token) = std::env::var("WONDERLAND_SERVER_TOKEN") {
            if !token.is_empty()
                && request
                    .headers()
                    .get("authorization")
                    .and_then(|h| h.to_str().ok())
                    != Some(format!("Bearer {token}").as_str())
            {
                return Err(StatusCode::UNAUTHORIZED);
            }
        }
    }
    Ok(next.run(request).await)
}

async fn proxy_management(
    State(state): State<AppState>,
    Path(path): Path<String>,
    request: axum::extract::Request,
) -> Result<Response, ApiError> {
    let client = state
        .subscription_client()
        .ok_or_else(|| ApiError::service_unavailable(cliproxy_not_ready_message()))?;
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, 16 * 1024 * 1024)
        .await
        .map_err(anyhow::Error::from)?;
    let response = client
        .management_api(
            parts.method,
            &path,
            parts.uri.query(),
            parts
                .headers
                .get("content-type")
                .and_then(|h| h.to_str().ok()),
            bytes.to_vec(),
        )
        .await?;
    let status = response.status();
    let content_type = response.headers().get("content-type").cloned();
    let mut builder = Response::builder().status(status);
    if let Some(content_type) = content_type {
        builder = builder.header("content-type", content_type);
    }
    Ok(builder
        .body(axum::body::Body::from_stream(response.bytes_stream()))
        .map_err(anyhow::Error::from)?)
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

async fn answer_permission(
    Path(id): Path<String>,
    Json(answer): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !crate::approval::answer(&id, answer["allow"] == true) {
        return Err(ApiError::not_found(
            "permission request expired or already answered",
        ));
    }
    Ok(Json(serde_json::json!({"status":"ok"})))
}

/// 流式运行 Agent：以 SSE 推送增量事件，最后一条响应帧带上完整结果。
///
/// 事件形状见 provider::StreamEvent，例如
/// data: {"type":"text_delta","text":"..."}
async fn run_agent_stream(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
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
        let handler: Arc<dyn crate::permissions::PermissionHandler> = if headers
            .get("x-wonderland-interactive")
            .is_some_and(|h| h == "true")
        {
            Arc::new(crate::approval::InteractiveHandler(sink.clone()))
        } else {
            Arc::new(crate::permissions::DenyAllHandler)
        };
        let run = runtime.run_with_events(request, handler, Some(sink));
        let outcome = tokio::select! {
            result = run => result,
            _ = frames.closed() => { forward.abort(); return; }
        };
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
    model: Option<String>,
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
        model: state.runtime.provider.default_model(),
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
        cliproxyapi_configured: state.subscription_client().is_some(),
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
        .subscription_client()
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
        .subscription_client()
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
        .subscription_client()
        .ok_or_else(|| ApiError::service_unavailable(cliproxy_not_ready_message()))?;
    Ok(Json(client.list_models().await?))
}

async fn verify_cliproxy(
    State(state): State<AppState>,
    Json(request): Json<CliProxyVerifyRequest>,
) -> Result<Json<crate::cliproxy::CliProxyVerification>, ApiError> {
    let client = state
        .subscription_client()
        .ok_or_else(|| ApiError::service_unavailable(cliproxy_not_ready_message()))?;
    Ok(Json(client.verify(request.model).await?))
}

#[derive(Debug, Deserialize)]
pub struct CliProxyLoginRequest {
    pub provider: String,
}

type LoginCache = tokio::sync::Mutex<
    std::collections::HashMap<String, (std::time::Instant, crate::cliproxy::CliProxyLoginStatus)>,
>;
fn login_cache() -> &'static LoginCache {
    static CACHE: std::sync::OnceLock<LoginCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

async fn start_cliproxy_login(
    State(state): State<AppState>,
    Json(request): Json<CliProxyLoginRequest>,
) -> Result<Json<crate::cliproxy::CliProxyLoginStart>, ApiError> {
    let client = state
        .subscription_client()
        .ok_or_else(|| ApiError::service_unavailable(cliproxy_not_ready_message()))?;
    let started = client.start_login(&request.provider).await?;
    if let Some(id) = started.state.clone() {
        login_cache()
            .lock()
            .await
            .retain(|_, (at, _)| at.elapsed() < std::time::Duration::from_secs(3600));
        tokio::spawn(async move {
            for _ in 0..300 {
                match client.login_status(&id).await {
                    Ok(status) if status.status != "wait" => {
                        login_cache()
                            .lock()
                            .await
                            .insert(id, (std::time::Instant::now(), status));
                        return;
                    }
                    _ => tokio::time::sleep(std::time::Duration::from_secs(2)).await,
                }
            }
        });
    }
    Ok(Json(started))
}

#[derive(Debug, Deserialize)]
pub struct CliProxyLoginQuery {
    pub state: String,
}

async fn cliproxy_login_status(
    State(state): State<AppState>,
    Query(query): Query<CliProxyLoginQuery>,
) -> Result<Json<crate::cliproxy::CliProxyLoginStatus>, ApiError> {
    if let Some((_, status)) = login_cache().lock().await.get(&query.state) {
        return Ok(Json(status.clone()));
    }
    let client = state
        .subscription_client()
        .ok_or_else(|| ApiError::service_unavailable(cliproxy_not_ready_message()))?;
    Ok(Json(client.login_status(&query.state).await?))
}

async fn cancel_cliproxy_login(
    State(state): State<AppState>,
    Query(query): Query<CliProxyLoginQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let client = state
        .subscription_client()
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
            let start = rendered.to_lowercase()[..position]
                .chars()
                .count()
                .saturating_sub(60)
                .min(chars.len());
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
async fn list_mcp_servers(State(state): State<AppState>) -> Json<Vec<serde_json::Value>> {
    let statuses = state.runtime.mcp_status.read().await.clone();
    if !statuses.is_empty() {
        return Json(statuses);
    }
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
    Json(
        servers
            .into_iter()
            .map(|s| serde_json::to_value(s).unwrap())
            .collect(),
    )
}

#[derive(Default, Deserialize)]
struct McpWorkspace {
    cwd: Option<String>,
}

async fn reload_mcp(
    State(state): State<AppState>,
    Json(workspace): Json<McpWorkspace>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let cwd = workspace
        .cwd
        .map(std::path::PathBuf::from)
        .unwrap_or(std::env::current_dir().map_err(anyhow::Error::from)?);
    let outcome = crate::mcp::load_tools(&cwd).await;
    let statuses = crate::mcp::summaries(&outcome);
    state.runtime.tools.replace_mcp(outcome.tools);
    *state.runtime.mcp_status.write().await = statuses.clone();
    Ok(Json(serde_json::json!({"servers":statuses})))
}
fn configured_mcp(
    name: &str,
    workspace: McpWorkspace,
) -> anyhow::Result<crate::mcp::McpServerConfig> {
    let cwd = workspace
        .cwd
        .map(std::path::PathBuf::from)
        .unwrap_or(std::env::current_dir().map_err(anyhow::Error::from)?);
    crate::mcp::load_server_configs(&cwd)
        .into_iter()
        .find(|(n, _)| n == name)
        .map(|(_, c)| c)
        .ok_or_else(|| anyhow::anyhow!("MCP server is not configured: {name}"))
}
async fn mcp_login(
    Path(name): Path<String>,
    Json(workspace): Json<McpWorkspace>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let config = configured_mcp(&name, workspace)?;
    let url = config
        .url
        .ok_or_else(|| anyhow::anyhow!("stdio MCP does not use HTTP OAuth"))?;
    Ok(Json(
        crate::mcp_oauth::start(&url, config.oauth.unwrap_or_default()).await?,
    ))
}
async fn mcp_logout(
    Path(name): Path<String>,
    Json(workspace): Json<McpWorkspace>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let config = configured_mcp(&name, workspace)?;
    crate::mcp_oauth::logout(&config.url.unwrap_or_default())?;
    Ok(Json(serde_json::json!({"status":"ok"})))
}
async fn mcp_login_status(
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    Json(crate::mcp_oauth::status(query.get("state").map(String::as_str).unwrap_or("")).await)
}
async fn model_profile(
    State(state): State<AppState>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let model = query
        .get("model")
        .filter(|m| !m.is_empty())
        .cloned()
        .or_else(|| state.runtime.provider.default_model())
        .unwrap_or_default();
    let profile = state.runtime.model_profile_for(&model);
    Json(
        serde_json::json!({"model":model,"supported_reasoning_efforts":crate::model_profile::supported_reasoning_efforts(&model),"name":profile.name,"context_window":profile.context_window,"max_output_tokens":profile.max_output_tokens,"protocol":profile.protocol,"reasoning":format!("{:?}",profile.reasoning),"cache":format!("{:?}",profile.cache),"edit_preference":format!("{:?}",profile.edit_preference)}),
    )
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
