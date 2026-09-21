//! Service-owned native workflows. HTTP clients observe runs; they never own them.
use crate::{
    api::AppState,
    native_executor::{self, NativeControl, NativeEvent, NativeRequest},
    workflow::{WorkflowCreate, WorkflowRecord, WorkflowStatus as Status, WorkflowStore},
};
use anyhow::{ensure, Context, Result};
use axum::{
    extract::{Path as HttpPath, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs::File,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::sync::{mpsc, watch, Mutex};

#[derive(Clone, Copy, PartialEq)]
enum PendingKind {
    Permission,
    Question,
}
struct Job {
    cancel: watch::Sender<bool>,
    controls: mpsc::Sender<NativeControl>,
    pending: HashMap<String, PendingKind>,
    cwd: std::path::PathBuf,
    read_only: bool,
}

pub struct WorkbenchService {
    pub store: Arc<WorkflowStore>,
    pub intelligence: crate::model_intelligence::ModelIntelligence,
    pub pricing: crate::pricing::PriceService,
    pub teams: crate::team_service::TeamService,
    jobs: Mutex<HashMap<String, Job>>,
    closing: AtomicBool,
    // OS releases ownership on crash. Holding this lock prevents a second service
    // using another HTTP port from declaring this service's running tasks dead.
    _ownership: File,
}

impl WorkbenchService {
    pub(crate) fn is_closing(&self) -> bool {
        self.closing.load(Ordering::SeqCst)
    }
    pub(crate) async fn children_running(&self, ids: &[String]) -> bool {
        let jobs = self.jobs.lock().await;
        ids.iter().any(|id| jobs.contains_key(id))
    }
    pub fn open(directory: &Path) -> Result<Arc<Self>> {
        std::fs::create_dir_all(directory)?;
        let ownership = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("workbench.lock"))?;
        ownership
            .try_lock()
            .context("another service owns this workbench data directory")?;
        let store = Arc::new(WorkflowStore::open(directory.join("workflows.sqlite"))?);
        store.recover_interrupted()?;
        let intelligence = crate::model_intelligence::ModelIntelligence::open(directory)?;
        let pricing = crate::pricing::PriceService::open(directory)?;
        let teams = crate::team_service::TeamService::open(directory)?;
        Ok(Arc::new(Self {
            store,
            intelligence,
            pricing,
            teams,
            jobs: Mutex::new(HashMap::new()),
            closing: AtomicBool::new(false),
            _ownership: ownership,
        }))
    }

    pub fn create(&self, mut request: WorkflowCreate) -> Result<WorkflowRecord> {
        ensure!(
            !self.closing.load(Ordering::SeqCst),
            "service is shutting down"
        );
        ensure!(
            native_executor::supports_native(&request.app_id),
            "this app currently supports direct terminal use only; no managed adapter is available"
        );
        ensure!(
            !request.model.trim().is_empty(),
            "choose an explicit model for a managed task"
        );
        if request.mode == "chat" {
            request.read_only = true;
        }
        let cwd =
            std::fs::canonicalize(&request.cwd).context("project directory is unavailable")?;
        ensure!(cwd.is_dir(), "project must be a directory");
        request.cwd = cwd.to_string_lossy().into_owned();
        let record = self.store.create(request)?;
        self.store.upsert_project(&record.cwd, None)?;
        Ok(record)
    }

    pub async fn start(self: &Arc<Self>, id: &str) -> Result<WorkflowRecord> {
        ensure!(
            !self
                .store
                .events(id, 0, 10)?
                .iter()
                .any(|event| event.kind == "team_owner"),
            "this child is owned by a collaboration; start its parent team"
        );
        self.start_with_effort(id, None).await
    }

    pub(crate) async fn start_with_effort(
        self: &Arc<Self>,
        id: &str,
        effort: Option<String>,
    ) -> Result<WorkflowRecord> {
        let mut jobs = self.jobs.lock().await;
        ensure!(
            !self.closing.load(Ordering::SeqCst),
            "service is shutting down"
        );
        let record = self.store.get(id)?.context("task not found")?;
        // A retry creates a new task and a fresh native session. Never replay edits
        // or swap the native session identity of a possibly partially completed run.
        ensure!(
            record.status == Status::Draft,
            "only a draft can start; create a new task to retry"
        );
        ensure!(
            native_executor::supports_native(&record.app_id),
            "managed adapter unavailable"
        );
        let cwd = std::fs::canonicalize(&record.cwd)?;
        ensure!(cwd.is_dir(), "project directory is unavailable");
        for job in jobs.values() {
            ensure!((record.read_only && job.read_only) || !(cwd.starts_with(&job.cwd) || job.cwd.starts_with(&cwd)), "another managed task owns an overlapping project; wait or use an isolated worktree");
        }
        let (cancel, cancel_rx) = watch::channel(false);
        let (controls, controls_rx) = mpsc::channel(32);
        self.store
            .append_event(id, "execution_settings", json!({"reasoning_effort":effort}))?;
        let record = self
            .store
            .transition(id, &[Status::Draft], Status::Running, None)?;
        jobs.insert(
            id.to_owned(),
            Job {
                cancel: cancel.clone(),
                controls,
                pending: HashMap::new(),
                cwd,
                read_only: record.read_only,
            },
        );
        let service = Arc::clone(self);
        let task = record.clone();
        tokio::spawn(async move {
            service
                .run(task, effort, cancel, cancel_rx, controls_rx)
                .await;
        });
        Ok(record)
    }

    async fn run(
        self: Arc<Self>,
        record: WorkflowRecord,
        effort: Option<String>,
        cancel: watch::Sender<bool>,
        cancel_rx: watch::Receiver<bool>,
        controls: mpsc::Receiver<NativeControl>,
    ) {
        let (tx, mut rx) = mpsc::channel(128);
        let request = NativeRequest {
            app_id: record.app_id,
            model: record.model,
            cwd: record.cwd.into(),
            prompt: record.prompt,
            read_only: record.read_only,
            max_duration_secs: record.max_duration_secs,
            reasoning_effort: effort,
            config_path: None,
        };
        let runner = tokio::spawn(native_executor::execute_with_control(
            request, tx, cancel_rx, controls,
        ));
        let mut persistence_error = None;
        while let Some(event) = rx.recv().await {
            if let Err(error) = self.record_event(&record.id, event).await {
                persistence_error
                    .get_or_insert_with(|| format!("cannot record execution evidence: {error}"));
                let _ = cancel.send(true);
            }
        }
        let outcome = runner.await;
        let mut jobs = self.jobs.lock().await;
        let cancelled = *cancel.borrow();
        let (status, detail) = if let Some(error) = persistence_error {
            (Status::Failed, Some(error))
        } else {
            match outcome {
                Ok(Ok(result)) if result.status == "completed" && !cancelled => (Status::Verifying, Some("Official tool finished. Inspect the output and record acceptance evidence.".into())),
                Ok(Ok(result)) if result.status == "cancelled" || cancelled => (Status::Cancelled, None),
                Ok(Ok(result)) => (Status::Failed, Some(format!("official execution ended: {}", result.status))),
                Ok(Err(_)) if cancelled => (Status::Cancelled, None),
                Ok(Err(error)) => (Status::Failed, Some(format!("{error:#}"))),
                Err(_) => (Status::Failed, Some("native worker stopped unexpectedly".into())),
            }
        };
        if let Err(error) = self.store.transition(
            &record.id,
            &[Status::Running, Status::WaitingInput],
            status,
            detail.as_deref(),
        ) {
            tracing::error!(task=%record.id, %error, "failed to persist native task outcome");
        }
        jobs.remove(&record.id);
    }

    async fn record_event(&self, id: &str, event: NativeEvent) -> Result<()> {
        let mut jobs = self.jobs.lock().await;
        let job = jobs
            .get_mut(id)
            .context("task execution no longer registered")?;
        let mut data = serde_json::to_value(&event)?;
        let kind = data["type"]
            .as_str()
            .context("event type missing")?
            .to_owned();
        if serde_json::to_vec(&data)?.len() > crate::workflow::MAX_EVENT_BYTES {
            data = json!({"type":kind,"truncated":true,"message":"Oversized event omitted; task output is retained up to its documented limit."});
        }
        self.store.append_event(id, &kind, data)?;
        match event {
            NativeEvent::TextDelta { text } => {
                self.store.append_output(id, &text)?;
            }
            NativeEvent::SessionStarted { session_id } => {
                self.store.set_session_id(id, &session_id)?;
            }
            NativeEvent::PermissionRequested { request_id, .. } => {
                job.pending.insert(request_id, PendingKind::Permission);
            }
            NativeEvent::QuestionRequested { request_id, .. } => {
                job.pending.insert(request_id, PendingKind::Question);
            }
            NativeEvent::PermissionResolved { request_id, .. } => {
                job.pending.remove(&request_id);
            }
            _ => {}
        }
        let current = self.store.get(id)?.context("task not found")?.status;
        let next = if job.pending.is_empty() {
            Status::Running
        } else {
            Status::WaitingInput
        };
        if current != next {
            self.store.transition(id, &[current], next, None)?;
        }
        Ok(())
    }

    pub async fn cancel(&self, id: &str) -> Result<WorkflowRecord> {
        let jobs = self.jobs.lock().await;
        let record = self.store.get(id)?.context("task not found")?;
        if let Some(job) = jobs.get(id) {
            self.store.append_event(id, "cancel_requested", json!({}))?;
            job.cancel.send(true).context("worker already stopped")?;
            return Ok(record); // Final cancelled status is recorded after process cleanup.
        }
        self.store.transition(
            id,
            &[record.status],
            Status::Cancelled,
            Some("Cancelled without an active process"),
        )
    }

    pub async fn control(&self, id: &str, control: NativeControl) -> Result<WorkflowRecord> {
        let mut jobs = self.jobs.lock().await;
        let job = jobs
            .get_mut(id)
            .context("no active execution for this task")?;
        ensure!(!*job.cancel.borrow(), "task is being cancelled");
        let (request_id, kind) = match &control {
            NativeControl::Permission { request_id, .. } => (request_id, PendingKind::Permission),
            NativeControl::Answer { request_id, .. } => (request_id, PendingKind::Question),
        };
        ensure!(
            job.pending.get(request_id) == Some(&kind),
            "stale or mismatched request; refresh the task events"
        );
        // Audit before sending. A failed delivery remains an explicit submission,
        // not an assertion that the official tool accepted the decision.
        self.store
            .append_event(id, "control_submitted", serde_json::to_value(&control)?)?;
        job.controls
            .try_send(control.clone())
            .context("native control channel is unavailable")?;
        job.pending.remove(request_id);
        let current = self.store.get(id)?.context("task not found")?;
        if job.pending.is_empty() && current.status == Status::WaitingInput {
            self.store
                .transition(id, &[Status::WaitingInput], Status::Running, None)
        } else {
            Ok(current)
        }
    }

    pub async fn accept(&self, id: &str, evidence: &str) -> Result<WorkflowRecord> {
        let jobs = self.jobs.lock().await;
        ensure!(!jobs.contains_key(id), "execution has not settled");
        ensure!(
            !self
                .store
                .events(id, 0, 10)?
                .iter()
                .any(|event| event.kind == "team_owner"),
            "this child requires its parent team's independent verification"
        );
        ensure!(
            !evidence.trim().is_empty() && evidence.len() <= 16_384,
            "provide acceptance evidence between 1 and 16384 bytes"
        );
        let record = self.store.get(id)?.context("task not found")?;
        ensure!(
            record.status == Status::Verifying,
            "only a task awaiting verification can be accepted"
        );
        self.store.append_event(id, "human_verification", json!({"evidence":evidence,"acceptance":record.acceptance,"method":"explicit_user_review"}))?;
        self.store.transition(
            id,
            &[Status::Verifying],
            Status::Succeeded,
            Some("Explicit user verification recorded"),
        )
    }

    pub async fn shutdown(&self) {
        self.closing.store(true, Ordering::SeqCst);
        self.teams.stop_all().await;
        for job in self.jobs.lock().await.values() {
            let _ = job.cancel.send(true);
        }
        // Native runner has bounded cancellation and process-tree cleanup.
        for _ in 0..100 {
            if self.jobs.lock().await.is_empty() && self.teams.is_idle().await {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .merge(crate::team_service::routes())
        .route("/api/v1/apps", get(apps))
        .route("/api/v1/intelligence", get(intelligence))
        .route("/api/v1/intelligence/refresh", post(refresh_intelligence))
        .route("/api/v1/workflows", get(list).post(create))
        .route("/api/v1/workflows/{id}", get(detail))
        .route("/api/v1/workflows/{id}/events", get(events))
        .route("/api/v1/workflows/{id}/start", post(start))
        .route("/api/v1/workflows/{id}/cancel", post(cancel))
        .route("/api/v1/workflows/{id}/approve", post(approve))
        .route("/api/v1/workflows/{id}/answer", post(answer))
        .route("/api/v1/workflows/{id}/accept", post(accept))
        .route("/api/v1/projects", get(projects).post(project))
}

struct ServiceError(anyhow::Error);
impl<E: Into<anyhow::Error>> From<E> for ServiceError {
    fn from(error: E) -> Self {
        Self(error.into())
    }
}
impl IntoResponse for ServiceError {
    fn into_response(self) -> Response {
        (
            StatusCode::CONFLICT,
            Json(json!({"error":self.0.to_string()})),
        )
            .into_response()
    }
}
type ApiResult = std::result::Result<Json<Value>, ServiceError>;
async fn apps() -> Json<Value> {
    Json(json!(crate::desktop_bridge::official_apps()))
}
async fn intelligence(State(s): State<AppState>) -> Json<Value> {
    let mut status = s.workbench.intelligence.status();
    status["pricing"] = s.workbench.pricing.status();
    Json(status)
}
async fn refresh_intelligence(State(s): State<AppState>) -> ApiResult {
    Ok(Json(json!(s.workbench.intelligence.refresh().await?)))
}
async fn list(State(s): State<AppState>) -> ApiResult {
    Ok(Json(json!(s.workbench.store.list()?)))
}
async fn create(State(s): State<AppState>, Json(request): Json<WorkflowCreate>) -> ApiResult {
    Ok(Json(json!(s.workbench.create(request)?)))
}
async fn detail(State(s): State<AppState>, HttpPath(id): HttpPath<String>) -> ApiResult {
    Ok(Json(json!(s
        .workbench
        .store
        .get(&id)?
        .context("task not found")?)))
}
#[derive(Deserialize)]
struct Cursor {
    #[serde(default)]
    after: i64,
    #[serde(default = "page_size")]
    limit: usize,
}
fn page_size() -> usize {
    200
}
async fn events(
    State(s): State<AppState>,
    HttpPath(id): HttpPath<String>,
    Query(q): Query<Cursor>,
) -> ApiResult {
    Ok(Json(json!(s
        .workbench
        .store
        .events(&id, q.after, q.limit)?)))
}
async fn start(State(s): State<AppState>, HttpPath(id): HttpPath<String>) -> ApiResult {
    Ok(Json(json!(s.workbench.start(&id).await?)))
}
async fn cancel(State(s): State<AppState>, HttpPath(id): HttpPath<String>) -> ApiResult {
    Ok(Json(json!(s.workbench.cancel(&id).await?)))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Approval {
    request_id: String,
    approve: bool,
}
async fn approve(
    State(s): State<AppState>,
    HttpPath(id): HttpPath<String>,
    Json(r): Json<Approval>,
) -> ApiResult {
    Ok(Json(json!(
        s.workbench
            .control(
                &id,
                NativeControl::Permission {
                    request_id: r.request_id,
                    approve: r.approve
                }
            )
            .await?
    )))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Answer {
    request_id: String,
    answers: Value,
}
async fn answer(
    State(s): State<AppState>,
    HttpPath(id): HttpPath<String>,
    Json(r): Json<Answer>,
) -> ApiResult {
    Ok(Json(json!(
        s.workbench
            .control(
                &id,
                NativeControl::Answer {
                    request_id: r.request_id,
                    answers: r.answers
                }
            )
            .await?
    )))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Acceptance {
    evidence: String,
}
async fn accept(
    State(s): State<AppState>,
    HttpPath(id): HttpPath<String>,
    Json(r): Json<Acceptance>,
) -> ApiResult {
    Ok(Json(json!(s.workbench.accept(&id, &r.evidence).await?)))
}
async fn projects(State(s): State<AppState>) -> ApiResult {
    Ok(Json(json!(s.workbench.store.list_projects()?)))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Project {
    cwd: String,
    name: Option<String>,
}
async fn project(State(s): State<AppState>, Json(r): Json<Project>) -> ApiResult {
    let cwd = std::fs::canonicalize(&r.cwd)?;
    ensure_directory(&cwd)?;
    Ok(Json(json!(s.workbench.store.upsert_project(
        &cwd.to_string_lossy(),
        r.name.as_deref()
    )?)))
}
fn ensure_directory(path: &Path) -> Result<()> {
    ensure!(path.is_dir(), "project must be a directory");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn draft(directory: &Path) -> WorkflowCreate {
        WorkflowCreate {
            prompt: "test".into(),
            cwd: directory.to_string_lossy().into_owned(),
            app_id: "codex".into(),
            model: "test-model".into(),
            ..Default::default()
        }
    }
    #[test]
    fn ownership_prevents_false_recovery_and_releases_on_close() {
        let dir = tempfile::tempdir().unwrap();
        let service = WorkbenchService::open(dir.path()).unwrap();
        let record = service.create(draft(dir.path())).unwrap();
        service
            .store
            .transition(&record.id, &[Status::Draft], Status::Running, None)
            .unwrap();
        assert!(WorkbenchService::open(dir.path()).is_err());
        assert_eq!(
            service.store.get(&record.id).unwrap().unwrap().status,
            Status::Running
        );
        drop(service);
        let service = WorkbenchService::open(dir.path()).unwrap();
        assert_eq!(
            service.store.get(&record.id).unwrap().unwrap().status,
            Status::Interrupted
        );
    }
    #[tokio::test]
    async fn completion_requires_independent_evidence_and_rejects_stale_controls() {
        let dir = tempfile::tempdir().unwrap();
        let s = WorkbenchService::open(dir.path()).unwrap();
        let record = s.create(draft(dir.path())).unwrap();
        assert!(s.accept(&record.id, "looks good").await.is_err());
        s.store
            .transition(&record.id, &[Status::Draft], Status::Running, None)
            .unwrap();
        s.store
            .transition(&record.id, &[Status::Running], Status::Verifying, None)
            .unwrap();
        assert!(s.accept(&record.id, "  ").await.is_err());
        assert!(s
            .control(
                &record.id,
                NativeControl::Permission {
                    request_id: "stale".into(),
                    approve: true
                }
            )
            .await
            .is_err());
        assert_eq!(
            s.accept(&record.id, "Reviewed the resulting diff and test output")
                .await
                .unwrap()
                .status,
            Status::Succeeded
        );
    }
    #[test]
    fn chat_is_read_only_and_unknown_apps_cannot_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let s = WorkbenchService::open(dir.path()).unwrap();
        let mut request = draft(dir.path());
        request.mode = "chat".into();
        assert!(s.create(request.clone()).unwrap().read_only);
        request.app_id = "wonderland".into();
        assert!(s.create(request).is_err());
    }
    #[tokio::test]
    async fn cancellation_waits_for_worker_cleanup_and_permissions_are_one_time() {
        let dir = tempfile::tempdir().unwrap();
        let s = WorkbenchService::open(dir.path()).unwrap();
        let r = s.create(draft(dir.path())).unwrap();
        s.store
            .transition(&r.id, &[Status::Draft], Status::Running, None)
            .unwrap();
        let (cancel, rx) = watch::channel(false);
        let (tx, mut controls) = mpsc::channel(4);
        s.jobs.lock().await.insert(
            r.id.clone(),
            Job {
                cancel,
                controls: tx,
                pending: HashMap::new(),
                cwd: std::fs::canonicalize(dir.path()).unwrap(),
                read_only: false,
            },
        );
        s.record_event(
            &r.id,
            NativeEvent::PermissionRequested {
                request_id: "r1".into(),
                description: "write".into(),
                data: json!({}),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            s.store.get(&r.id).unwrap().unwrap().status,
            Status::WaitingInput
        );
        let decision = NativeControl::Permission {
            request_id: "r1".into(),
            approve: false,
        };
        s.control(&r.id, decision.clone()).await.unwrap();
        assert!(matches!(
            controls.recv().await.unwrap(),
            NativeControl::Permission { approve: false, .. }
        ));
        assert!(s.control(&r.id, decision).await.is_err());
        assert_eq!(s.cancel(&r.id).await.unwrap().status, Status::Running);
        assert!(*rx.borrow());
        assert!(s.accept(&r.id, "unverified").await.is_err());
    }
    #[tokio::test]
    async fn overlapping_writers_cannot_start_and_ui_disconnect_does_not_own_output() {
        let dir = tempfile::tempdir().unwrap();
        let s = WorkbenchService::open(dir.path()).unwrap();
        let first = s.create(draft(dir.path())).unwrap();
        let second = s.create(draft(dir.path())).unwrap();
        s.store
            .transition(&first.id, &[Status::Draft], Status::Running, None)
            .unwrap();
        let (cancel, _) = watch::channel(false);
        let (tx, _) = mpsc::channel(1);
        s.jobs.lock().await.insert(
            first.id.clone(),
            Job {
                cancel,
                controls: tx,
                pending: HashMap::new(),
                cwd: std::fs::canonicalize(dir.path()).unwrap(),
                read_only: false,
            },
        );
        assert!(s
            .start(&second.id)
            .await
            .unwrap_err()
            .to_string()
            .contains("overlapping"));
        assert_eq!(
            s.store.get(&second.id).unwrap().unwrap().status,
            Status::Draft
        );
        s.record_event(
            &first.id,
            NativeEvent::TextDelta {
                text: "result without any UI subscriber".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            s.store.get(&first.id).unwrap().unwrap().output,
            "result without any UI subscriber"
        );
    }
}
