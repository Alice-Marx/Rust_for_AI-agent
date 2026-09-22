//! Durable DAG execution through native harnesses. The backend owns execution,
//! permission handling and independent verification; UI disconnects do not cancel it.
use crate::{
    api::AppState,
    native_executor,
    routing::RoutingPreviewRequest,
    team_store::{
        ExecutorBinding, NodeSpec, NodeStatus, TeamCreate, TeamRecord, TeamStatus, TeamStore,
        TeamStrategy,
    },
    workbench_service::WorkbenchService,
    workflow::{WorkflowCreate, WorkflowRecord, WorkflowStatus},
};
use anyhow::{bail, ensure, Context, Result};
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
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    sync::{watch, Mutex},
    task::JoinSet,
    time::{Duration, Instant},
};

struct TeamJob {
    cancel: watch::Sender<bool>,
    children: Vec<String>,
}
pub struct TeamService {
    pub store: TeamStore,
    directory: PathBuf,
    jobs: Mutex<HashMap<String, TeamJob>>,
}

impl TeamService {
    pub fn open(directory: &Path) -> Result<Self> {
        let store = TeamStore::open(directory.join("teams.sqlite"))?;
        store.recover_interrupted()?;
        let directory = directory.join("team-workspaces");
        std::fs::create_dir_all(&directory)?;
        Ok(Self {
            store,
            directory: std::fs::canonicalize(directory)?,
            jobs: Mutex::new(HashMap::new()),
        })
    }
    pub fn create(&self, mut request: TeamCreate) -> Result<TeamRecord> {
        request.cwd = std::fs::canonicalize(&request.cwd)
            .context("team project is unavailable")?
            .to_string_lossy()
            .into_owned();
        ensure!(
            Path::new(&request.cwd).is_dir(),
            "team project must be a directory"
        );
        validate_binding(&request.planner)?;
        ensure!(
            request.strategy != TeamStrategy::Assigned || !request.nodes.is_empty(),
            "assigned execution requires an explicit node plan with an executor on every node"
        );
        for candidate in &request.candidates {
            validate_binding(candidate)?;
        }
        for node in &request.nodes {
            if let Some(binding) = &node.executor {
                validate_binding(binding)?;
            }
        }
        self.store.create(request)
    }
    pub async fn start(service: &Arc<WorkbenchService>, id: &str) -> Result<TeamRecord> {
        let mut jobs = service.teams.jobs.lock().await;
        ensure!(!service.is_closing(), "service is shutting down");
        ensure!(!jobs.contains_key(id), "team is already running");
        let record = service.teams.store.get(id)?.context("team not found")?;
        ensure!(
            record.status == TeamStatus::Planned
                || (record.status == TeamStatus::Blocked && record.run_workspace.is_none()),
            "create a new team to retry a run with execution evidence"
        );
        let record = service.teams.store.transition(
            id,
            &[TeamStatus::Planned, TeamStatus::Blocked],
            TeamStatus::Running,
            None,
        )?;
        let (cancel, rx) = watch::channel(false);
        jobs.insert(
            id.into(),
            TeamJob {
                cancel,
                children: vec![],
            },
        );
        let service = Arc::clone(service);
        let id = id.to_owned();
        tokio::spawn(async move {
            let result = execute_team(Arc::clone(&service), &id, rx.clone()).await;
            // Cancel and reap every registered native child before recording a terminal
            // team status, even if persistence, merging or the scheduler itself failed.
            service.teams.stop_children(&service, &id).await;
            let current = service.teams.store.get(&id);
            if let Ok(Some(record)) = current {
                if record.status.is_active() {
                    let (status, detail) = match result {
                        Ok(()) if !*rx.borrow() => (TeamStatus::Succeeded, None),
                        _ if *rx.borrow() => (
                            TeamStatus::Cancelled,
                            Some("Cancellation completed; child processes have stopped".to_owned()),
                        ),
                        Err(error) => (TeamStatus::Failed, Some(short_error(&error))),
                        Ok(()) => (TeamStatus::Cancelled, None),
                    };
                    if let Err(error) = service.teams.store.transition(
                        &id,
                        &[record.status],
                        status,
                        detail.as_deref(),
                    ) {
                        tracing::error!(team_id=%id,%error,"could not finalize collaboration");
                    }
                }
            }
            service.teams.jobs.lock().await.remove(&id);
        });
        Ok(record)
    }
    pub async fn cancel(&self, id: &str) -> Result<TeamRecord> {
        let jobs = self.jobs.lock().await;
        if let Some(job) = jobs.get(id) {
            self.store
                .append_event(id, "cancellation_requested", json!({}))?;
            job.cancel
                .send(true)
                .context("team runner is unavailable")?;
            return self.store.get(id)?.context("team not found");
        }
        self.store.transition(
            id,
            &[TeamStatus::Planned, TeamStatus::Blocked],
            TeamStatus::Cancelled,
            None,
        )
    }
    pub async fn stop_all(&self) {
        for job in self.jobs.lock().await.values() {
            let _ = job.cancel.send(true);
        }
    }
    pub async fn is_idle(&self) -> bool {
        self.jobs.lock().await.is_empty()
    }
    async fn register_child(&self, id: &str, child: &str) -> Result<()> {
        let mut jobs = self.jobs.lock().await;
        let job = jobs.get_mut(id).context("team job is unavailable")?;
        ensure!(!*job.cancel.borrow(), "team cancelled");
        job.children.push(child.into());
        Ok(())
    }
    async fn stop_children(&self, service: &WorkbenchService, id: &str) {
        let children = self
            .jobs
            .lock()
            .await
            .get(id)
            .map(|job| job.children.clone())
            .unwrap_or_default();
        for child in &children {
            if service.store.get(child).ok().flatten().is_some_and(|r| {
                matches!(
                    r.status,
                    WorkflowStatus::Running | WorkflowStatus::WaitingInput
                )
            }) {
                let _ = service.cancel(child).await;
            }
        }
        // Do not declare completion while a child still owns a working directory.
        loop {
            if !service.children_running(&children).await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

fn short_error(error: &anyhow::Error) -> String {
    let mut text = format!("{error:#}");
    if text.len() > 12000 {
        let mut end = 12000;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}
fn validate_binding(binding: &ExecutorBinding) -> Result<()> {
    crate::team_store::validate_executor(binding)?;
    native_executor::validate_binding(
        &binding.app_id,
        &binding.model,
        binding.reasoning_effort.as_deref(),
        false,
    )?;
    ensure!(
        native_executor::supports_native(&binding.app_id),
        "{} has no managed native adapter; choose its direct terminal entry",
        binding.app_id
    );
    // Native adapters validate the provider, exact model and effort again before
    // process startup. No model family guessing or direct API fallback here.
    Ok(())
}
fn active_check(cancel: &watch::Receiver<bool>, deadline: Instant) -> Result<()> {
    ensure!(!*cancel.borrow(), "team cancelled");
    ensure!(Instant::now() < deadline, "team deadline exceeded");
    Ok(())
}

async fn execute_team(
    service: Arc<WorkbenchService>,
    id: &str,
    cancel: watch::Receiver<bool>,
) -> Result<()> {
    let mut record = service.teams.store.get(id)?.context("team not found")?;
    let deadline = Instant::now() + Duration::from_secs(record.request.max_duration_secs);
    if record.request.strategy == TeamStrategy::Automatic {
        // Each routing epoch fetches both sources anew. Cached display data cannot
        // silently authorize a routing decision after a failed online refresh.
        let refresh =
            async { tokio::join!(service.intelligence.refresh(), service.pricing.refresh()) };
        let mut cancelled = cancel.clone();
        let (bench, prices) = tokio::select! {
            result=refresh=>result,
            _=cancelled.changed()=>bail!("team cancelled"),
            _=tokio::time::sleep_until(deadline)=>bail!("team deadline exceeded while refreshing routing evidence"),
        };
        let evidence = json!({
            "livebench_snapshot":bench.as_ref().ok().map(|s|s.id.clone()),
            "livebench_error":bench.as_ref().err().map(short_error),
            "price_snapshot":prices.as_ref().ok().map(|s|s.id.clone()),
            "price_error":prices.as_ref().err().map(short_error),
            "candidates":record.request.candidates,
            "decision":"blocked",
            "reason":"Official tool account billing channel, subscription quota and exact LiveBench model/effort mappings are not yet attested. API list prices cannot price native subscription usage. Choose fixed execution without a USD cap until these identities are verified."
        });
        service
            .teams
            .store
            .append_event(id, "routing_epoch", evidence.clone())?;
        active_check(&cancel, deadline)?;
        service.teams.store.transition(
            id,
            &[TeamStatus::Running],
            TeamStatus::Blocked,
            evidence["reason"].as_str(),
        )?;
        return Ok(());
    }
    if record.request.budget_usd.is_some() {
        service.teams.store.transition(id,&[TeamStatus::Running],TeamStatus::Blocked,Some("The native tool's billing channel and spending limit cannot yet be verified. A USD budget cannot be enforced; choose fixed execution without a USD cap, or wait for billing attestation."))?;
        return Ok(());
    }
    active_check(&cancel, deadline)?;
    let source = PathBuf::from(&record.request.cwd);
    let root = service.teams.directory.clone();
    let run_id = id.to_owned();
    let workspace = tokio::task::spawn_blocking(move || {
        crate::team_workspace::prepare_run(&source, &root, &run_id)
    })
    .await??;
    record = service.teams.store.set_workspace(
        id,
        &workspace.source_revision,
        &workspace.integration.to_string_lossy(),
    )?;
    service.teams.store.append_event(id,"workspace_ready",json!({"path":workspace.integration,"starting_revision":workspace.source_revision,"isolation":"separate Git workspace, not an OS security sandbox"}))?;
    active_check(&cancel, deadline)?;
    if record.nodes.is_empty() {
        let planner = record.request.planner.clone();
        let child = create_child(
            &service,
            id,
            &planner,
            &workspace.integration,
            planning_prompt(&record),
            true,
            deadline,
            "Planner",
            vec!["Return a validated dependency plan".into()],
        )?;
        service.teams.store.append_event(
            id,
            "planner_workflow",
            json!({"workflow_id":child.id,"executor":planner}),
        )?;
        service.teams.register_child(id, &child.id).await?;
        service.start_team_child(&child.id, id).await?;
        let result = wait_child(&service, id, &child.id, cancel.clone(), deadline).await?;
        let nodes = parse_plan(&result.output)?;
        let current = service.teams.store.get(id)?.context("team disappeared")?;
        record = service.teams.store.set_plan(id, current.revision, nodes)?;
        service.store.append_event(&child.id,"plan_validated",json!({"team_id":id,"node_count":record.nodes.len(),"method":"host_schema_and_dag_validation"}))?;
        service.store.transition(
            &child.id,
            &[WorkflowStatus::Verifying],
            WorkflowStatus::Succeeded,
            None,
        )?;
    }
    let workspace = Arc::new(workspace);
    let git_gate = Arc::new(Mutex::new(()));
    let (worker_cancel, worker_rx) = watch::channel(false);
    let mut running = JoinSet::new();
    let mut failure = None;
    loop {
        if let Err(error) = active_check(&cancel, deadline) {
            failure = Some(error);
            break;
        }
        record = match service
            .teams
            .store
            .get(id)
            .and_then(|r| r.context("team disappeared"))
        {
            Ok(record) => record,
            Err(error) => {
                failure = Some(error);
                break;
            }
        };
        if record
            .nodes
            .iter()
            .all(|n| n.status == NodeStatus::Succeeded)
        {
            break;
        }
        for node in &record.nodes {
            if record.status == TeamStatus::WaitingInput {
                break;
            }
            if running.len() >= record.request.max_parallel {
                break;
            }
            if !(node.status == NodeStatus::Pending
                || (node.status == NodeStatus::Failed
                    && node.attempts.len() < (record.request.max_attempts as usize)))
            {
                continue;
            }
            if !node.spec.dependencies.iter().all(|parent| {
                record
                    .nodes
                    .iter()
                    .any(|n| &n.spec.id == parent && n.status == NodeStatus::Succeeded)
            }) {
                continue;
            }
            let binding = node
                .spec
                .executor
                .clone()
                .unwrap_or_else(|| record.request.planner.clone());
            let attempt =
                match service
                    .teams
                    .store
                    .begin_attempt(id, &node.spec.id, binding.clone(), None)
                {
                    Ok(attempt) => attempt,
                    Err(error) if !running.is_empty() => {
                        tracing::debug!(%error,"node will wait for current write scopes");
                        continue;
                    }
                    Err(error) => {
                        failure = Some(error);
                        break;
                    }
                };
            let service = Arc::clone(&service);
            let workspace = Arc::clone(&workspace);
            let git_gate = Arc::clone(&git_gate);
            let spec = node.spec.clone();
            let id = id.to_owned();
            let cancel = worker_rx.clone();
            running.spawn(async move {
                let result = execute_node(
                    Arc::clone(&service),
                    &id,
                    &workspace,
                    git_gate,
                    &spec,
                    attempt.number,
                    &binding,
                    cancel,
                    deadline,
                )
                .await;
                if let Err(error) = &result {
                    let _ = service.teams.store.set_attempt_status(
                        &id,
                        &spec.id,
                        attempt.number,
                        NodeStatus::Failed,
                        Some(&short_error(error)),
                        None,
                    );
                    if let Ok(Some(record)) = service.teams.store.get(&id) {
                        if let Some(child) = record
                            .nodes
                            .iter()
                            .find(|n| n.spec.id == spec.id)
                            .and_then(|n| n.attempts.last())
                            .and_then(|a| a.workflow_id.as_ref())
                        {
                            if service
                                .store
                                .get(child)
                                .ok()
                                .flatten()
                                .is_some_and(|r| r.status == WorkflowStatus::Verifying)
                            {
                                let _ = service.store.transition(
                                    child,
                                    &[WorkflowStatus::Verifying],
                                    WorkflowStatus::Failed,
                                    Some(&short_error(error)),
                                );
                            }
                        }
                    }
                }
                result
            });
        }
        if failure.is_some() {
            break;
        }
        if running.is_empty() {
            bail!("no eligible nodes remain; inspect failed dependencies");
        }
        let mut cancelled = cancel.clone();
        tokio::select! {
            result=running.join_next()=>match result {
                Some(Ok(Ok(())))=>{},
                Some(Ok(Err(error)))=>{
                    let current=match service.teams.store.get(id).and_then(|r|r.context("team disappeared")) {
                        Ok(record)=>record,Err(error)=>{failure=Some(error);break;}
                    };
                    if current.nodes.iter().any(|n|n.status==NodeStatus::Failed&&n.attempts.len()>=(current.request.max_attempts as usize)) {
                        failure=Some(error);break;
                    }
                    if let Err(error)=service.teams.store.append_event(id,"retry_scheduled",json!({"reason":short_error(&error),"executor_policy":"same pinned executor; fresh isolated attempt"})) {failure=Some(error);break;}
                },
                Some(Err(error))=>{failure=Some(error.into());break;},
                None=>bail!("team worker set unexpectedly empty"),
            },
            _=cancelled.changed()=>{failure=Some(anyhow::anyhow!("team cancelled"));break;},
            _=tokio::time::sleep_until(deadline)=>{failure=Some(anyhow::anyhow!("team deadline exceeded"));break;},
        }
    }
    if let Some(error) = failure {
        // Let each worker stop its harness and finish any already-started Git
        // transaction. Aborting futures could leave a blocking integration running
        // after the parent has been recorded as terminal.
        let _ = worker_cancel.send(true);
        while running.join_next().await.is_some() {}
        service.teams.stop_children(&service, id).await;
        return Err(error);
    }
    active_check(&cancel, deadline)?;
    service
        .teams
        .store
        .transition(id, &[TeamStatus::Running], TeamStatus::Verifying, None)?;
    let check_workspace = Arc::clone(&workspace);
    let revision = tokio::task::spawn_blocking(move || {
        crate::team_workspace::assert_clean(&check_workspace.integration)?;
        crate::team_workspace::current_revision(&check_workspace.integration)
    })
    .await??;
    let results = crate::team_checks::run_checks(
        &record.request.checks,
        &workspace.integration,
        cancel.clone(),
        deadline,
    )
    .await?;
    let passed = results.iter().all(|check| check.success);
    service.teams.store.record_verification(id, results)?;
    active_check(&cancel, deadline)?;
    let verified_workspace = Arc::clone(&workspace);
    let expected_revision = revision.clone();
    tokio::task::spawn_blocking(move || {
        crate::team_workspace::assert_clean(&verified_workspace.integration)?;
        ensure!(
            crate::team_workspace::current_revision(&verified_workspace.integration)?
                == expected_revision,
            "verification changed the tested Git revision"
        );
        crate::team_workspace::assert_source_unchanged(&verified_workspace)
    })
    .await??;
    active_check(&cancel, deadline)?;
    service.teams.store.append_event(id,"verification_artifact",json!({"workspace":workspace.integration,"tested_revision":revision,"method":"user_declared_host_commands","passed":passed,"source_checkout_modified":false}))?;
    ensure!(
        passed,
        "independent acceptance commands failed; inspect verification logs"
    );
    // Every implementation child becomes successful only after aggregate tests.
    for node in &service
        .teams
        .store
        .get(id)?
        .context("team disappeared")?
        .nodes
    {
        active_check(&cancel, deadline)?;
        for attempt in &node.attempts {
            if attempt.status != NodeStatus::Succeeded {
                continue;
            }
            if let Some(child) = &attempt.workflow_id {
                if service
                    .store
                    .get(child)?
                    .is_some_and(|r| r.status == WorkflowStatus::Verifying)
                {
                    service.store.append_event(child,"team_verification",json!({"team_id":id,"tested_revision":revision,"method":"user_declared_host_commands"}))?;
                    service.store.transition(
                        child,
                        &[WorkflowStatus::Verifying],
                        WorkflowStatus::Succeeded,
                        None,
                    )?;
                }
            }
        }
    }
    active_check(&cancel, deadline)?;
    Ok(())
}

fn create_child(
    service: &WorkbenchService,
    team_id: &str,
    binding: &ExecutorBinding,
    cwd: &Path,
    prompt: String,
    read_only: bool,
    deadline: Instant,
    title: &str,
    acceptance: Vec<String>,
) -> Result<WorkflowRecord> {
    ensure!(Instant::now() < deadline, "team deadline exceeded");
    let child = service.create(WorkflowCreate {
        title: format!("{title} · {team_id}"),
        prompt,
        cwd: cwd.to_string_lossy().into_owned(),
        mode: if read_only { "chat" } else { "work" }.into(),
        app_id: binding.app_id.clone(),
        model: binding.model.clone(),
        reasoning_effort: binding.reasoning_effort.clone(),
        read_only,
        max_duration_secs: deadline
            .saturating_duration_since(Instant::now())
            .as_secs()
            .max(1),
        acceptance,
    })?;
    service.store.append_event(
        &child.id,
        "team_owner",
        json!({"team_id":team_id,"executor":binding}),
    )?;
    Ok(child)
}
async fn wait_child(
    service: &WorkbenchService,
    team_id: &str,
    child_id: &str,
    mut cancel: watch::Receiver<bool>,
    deadline: Instant,
) -> Result<WorkflowRecord> {
    let mut stopping = false;
    loop {
        let child = service
            .store
            .get(child_id)?
            .context("child workflow disappeared")?;
        if (*cancel.borrow() || Instant::now() >= deadline) && !stopping {
            stopping = true;
            if matches!(
                child.status,
                WorkflowStatus::Running | WorkflowStatus::WaitingInput
            ) {
                service.cancel(child_id).await?;
            }
        }
        match child.status {
            WorkflowStatus::Verifying if !stopping => {
                resume_if_ready(service, team_id).await?;
                return Ok(child);
            }
            WorkflowStatus::Running | WorkflowStatus::WaitingInput => {
                let waiting = child.status == WorkflowStatus::WaitingInput;
                let current = service
                    .teams
                    .store
                    .get(team_id)?
                    .context("team disappeared")?;
                // A team-level state aggregates pending human questions. Child UI
                // and CLI remain the sole authority for permission decisions.
                if waiting && current.status == TeamStatus::Running {
                    let _ = service.teams.store.transition(
                        team_id,
                        &[TeamStatus::Running],
                        TeamStatus::WaitingInput,
                        None,
                    );
                } else if !waiting && current.status == TeamStatus::WaitingInput {
                    // Other concurrent children may still wait; only resume when none do.
                    resume_if_ready(service, team_id).await?;
                }
            }
            _ => {
                resume_if_ready(service, team_id).await?;
                bail!(
                    "child {child_id} ended with {}: {}",
                    child.status,
                    child.error.as_deref().unwrap_or("no additional detail")
                )
            }
        }
        if stopping {
            tokio::time::sleep(Duration::from_millis(100)).await;
        } else {
            tokio::select! {_=tokio::time::sleep(Duration::from_millis(150))=>{},_=cancel.changed()=>{}}
        }
    }
}

async fn resume_if_ready(service: &WorkbenchService, id: &str) -> Result<()> {
    let children = service
        .teams
        .jobs
        .lock()
        .await
        .get(id)
        .map(|j| j.children.clone())
        .unwrap_or_default();
    if !children.iter().any(|id| {
        service
            .store
            .get(id)
            .ok()
            .flatten()
            .is_some_and(|r| r.status == WorkflowStatus::WaitingInput)
    }) {
        if service
            .teams
            .store
            .get(id)?
            .is_some_and(|r| r.status == TeamStatus::WaitingInput)
        {
            let _ = service.teams.store.transition(
                id,
                &[TeamStatus::WaitingInput],
                TeamStatus::Running,
                None,
            );
        }
    }
    Ok(())
}

fn planning_prompt(record: &TeamRecord) -> String {
    format!("You are the planner for a bounded development task. Inspect this isolated project using read-only tools. Split the request into 1 to 12 small dependent tasks. Return ONLY a JSON object with a nodes array. Each node must have id (ASCII letters/digits/hyphen), objective, dependencies (node IDs), write_paths (literal project-relative files/directories, no globs, no '.' or '..'), acceptance (nonempty strings). Omit executor: every stage is pinned to the selected official tool/model. Do not change files or run code. Never change, delete, or weaken existing tests to make them pass. Include implementation and review work as useful. Read-only review nodes use empty write_paths. Tasks with overlapping write paths must depend on each other. Do not invent test commands: the host will run the user's checks.\n\nUser goal:\n{}\n\nChecks already selected by user:\n{}",record.request.prompt,serde_json::to_string(&record.request.checks).unwrap_or_default())
}
fn parse_plan(output: &str) -> Result<Vec<NodeSpec>> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Plan {
        nodes: Vec<NodeSpec>,
    }
    let text = output.trim();
    let text = if let Some(rest) = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
    {
        rest.strip_suffix("```")
            .context("planner returned an incomplete JSON fence")?
            .trim()
    } else {
        text
    };
    let plan: Plan = serde_json::from_str(text)
        .context("planner must return exactly one JSON plan; no prose or executable commands")?;
    crate::team_store::validate_plan(&plan.nodes)?;
    Ok(plan.nodes)
}

#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct ReviewReport {
    accepted: bool,
    findings: Vec<String>,
    summary: String,
}
fn parse_review(output: &str) -> Result<ReviewReport> {
    let text = output.trim();
    let text = if let Some(rest) = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
    {
        rest.strip_suffix("```")
            .context("incomplete review JSON")?
            .trim()
    } else {
        text
    };
    let report: ReviewReport = serde_json::from_str(text)
        .context("read-only review must return accepted/findings/summary JSON")?;
    ensure!(
        !report.summary.trim().is_empty()
            && report.summary.len() <= 8192
            && report.findings.len() <= 32
            && report.findings.iter().all(|f| f.len() <= 2048),
        "review report exceeds limits or has no summary"
    );
    Ok(report)
}

async fn execute_node(
    service: Arc<WorkbenchService>,
    id: &str,
    workspace: &Arc<crate::team_workspace::RunWorkspace>,
    git_gate: Arc<Mutex<()>>,
    spec: &NodeSpec,
    number: u32,
    binding: &ExecutorBinding,
    cancel: watch::Receiver<bool>,
    deadline: Instant,
) -> Result<()> {
    active_check(&cancel, deadline)?;
    let prepare_workspace = Arc::clone(workspace);
    let node_id = spec.id.clone();
    let guard = git_gate.lock().await;
    active_check(&cancel, deadline)?;
    let attempt = tokio::task::spawn_blocking(move || {
        crate::team_workspace::prepare_attempt(&prepare_workspace, &node_id, number)
    })
    .await??;
    drop(guard);
    let attempt_dir = attempt.path.clone();
    let record = service.teams.store.get(id)?.context("team disappeared")?;
    let dependencies:Vec<_>=record.nodes.iter().filter(|n|spec.dependencies.contains(&n.spec.id)).map(|n|json!({"node":n.spec.id,"artifacts":n.attempts.last().and_then(|a|a.artifact.clone())})).collect();
    let previous_errors: Vec<_> = record
        .nodes
        .iter()
        .filter(|n| n.spec.id == spec.id)
        .flat_map(|n| n.attempts.iter().filter_map(|a| a.error.clone()))
        .collect();
    let prompt=format!("Work only on the assigned subtask in this isolated project. Use only your selected official model/tool. Do not delegate to different models. Do not commit, change branches, touch .git, install hooks, or modify files outside the listed write paths. Existing tests and acceptance commands must not be weakened. This is a workspace boundary, not a security sandbox.\n\nOverall goal:\n{}\n\nSubtask:\n{}\n\nAllowed write paths: {}\nAcceptance: {}\nDependencies already integrated: {}\nPrior attempt errors: {}\n\nReturn a concise summary with changed files and any concerns. The host independently checks scope and runs acceptance commands.",record.request.prompt,spec.objective,serde_json::to_string(&spec.write_paths)?,serde_json::to_string(&spec.acceptance)?,serde_json::to_string(&dependencies)?,serde_json::to_string(&previous_errors)?);
    let prompt = if spec.write_paths.is_empty() {
        format!("{prompt}\nThis is a read-only acceptance review. Evaluate the full original goal, README and declared acceptance, not just whether the implementation claims success. Return ONLY JSON: {{\"accepted\":true or false,\"findings\":[blocking issues],\"summary\":\"short rationale\"}}. accepted may be true only with no blocking findings. Do not execute commands or edit files.")
    } else {
        prompt
    };
    let child = create_child(
        &service,
        id,
        binding,
        &attempt_dir,
        prompt,
        spec.write_paths.is_empty(),
        deadline,
        &spec.id,
        spec.acceptance.clone(),
    )?;
    service.teams.store.attach_child(
        id,
        &spec.id,
        number,
        &child.id,
        &attempt_dir.to_string_lossy(),
    )?;
    service.teams.register_child(id, &child.id).await?;
    service.start_team_child(&child.id, id).await?;
    let result = wait_child(&service, id, &child.id, cancel.clone(), deadline).await?;
    active_check(&cancel, deadline)?;
    ensure!(
        !result.output.trim().is_empty(),
        "official tool returned no task report"
    );
    if spec.write_paths.is_empty() {
        let review = parse_review(&result.output)?;
        service.teams.store.append_event(
            id,
            "review_report",
            json!({"node_id":spec.id,"workflow_id":child.id,"report":review}),
        )?;
        ensure!(
            review.accepted && review.findings.is_empty(),
            "review rejected the implementation: {}",
            review.findings.join("; ")
        );
    }
    service.teams.store.set_attempt_status(
        id,
        &spec.id,
        number,
        NodeStatus::Verifying,
        None,
        None,
    )?;
    let integrate_workspace = Arc::clone(workspace);
    let scopes = spec.write_paths.clone();
    let guard = git_gate.lock().await;
    active_check(&cancel, deadline)?;
    let (artifact, integration) = tokio::task::spawn_blocking(move || {
        let artifact = crate::team_workspace::capture_attempt(&attempt, &scopes)?;
        let integration =
            crate::team_workspace::integrate_attempt(&integrate_workspace, &artifact)?;
        Ok::<_, anyhow::Error>((artifact, integration))
    })
    .await??;
    drop(guard);
    active_check(&cancel, deadline)?;
    service.teams.store.set_attempt_status(id,&spec.id,number,NodeStatus::Succeeded,None,Some(json!({"git":artifact,"integration":integration,"workflow_id":child.id,"verification":"declared file scope and Git integration checked; final acceptance commands still required"})))?;
    Ok(())
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/teams", get(list).post(create))
        .route("/api/v1/teams/{id}", get(detail))
        .route("/api/v1/teams/{id}/events", get(events))
        .route("/api/v1/teams/{id}/start", post(start))
        .route("/api/v1/teams/{id}/cancel", post(cancel))
        .route("/api/v1/teams/{id}/routing/preview", post(routing_preview))
        .route("/api/v1/pricing", get(pricing))
        .route("/api/v1/pricing/refresh", post(refresh_pricing))
        .route("/api/v1/pricing/quote", get(quote))
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
            Json(json!({"error":short_error(&self.0)})),
        )
            .into_response()
    }
}
type ApiResult = std::result::Result<Json<Value>, ServiceError>;
async fn list(State(s): State<AppState>) -> ApiResult {
    Ok(Json(json!(s.workbench.teams.store.list()?)))
}
async fn create(State(s): State<AppState>, Json(request): Json<TeamCreate>) -> ApiResult {
    if s.workbench.is_closing() {
        return Err(anyhow::anyhow!("service is shutting down").into());
    }
    Ok(Json(json!(s.workbench.teams.create(request)?)))
}
async fn detail(State(s): State<AppState>, HttpPath(id): HttpPath<String>) -> ApiResult {
    Ok(Json(json!(s
        .workbench
        .teams
        .store
        .get(&id)?
        .context("team not found")?)))
}
async fn start(State(s): State<AppState>, HttpPath(id): HttpPath<String>) -> ApiResult {
    Ok(Json(json!(TeamService::start(&s.workbench, &id).await?)))
}
async fn cancel(State(s): State<AppState>, HttpPath(id): HttpPath<String>) -> ApiResult {
    Ok(Json(json!(s.workbench.teams.cancel(&id).await?)))
}
async fn routing_preview(
    State(s): State<AppState>,
    HttpPath(id): HttpPath<String>,
    Json(request): Json<RoutingPreviewRequest>,
) -> ApiResult {
    let team = s
        .workbench
        .teams
        .store
        .get(&id)?
        .context("team not found")?;
    if team.request.strategy != TeamStrategy::Automatic {
        return Err(
            anyhow::anyhow!("routing preview is only available for automatic teams").into(),
        );
    }
    let routing_request = request.into_request(&team.request.candidates);
    let (benchmark, pricing) = tokio::join!(
        s.workbench.intelligence.refresh(),
        s.workbench.pricing.refresh()
    );
    let benchmark = benchmark.context("LiveBench refresh failed")?;
    let pricing = pricing.context("official pricing refresh failed")?;
    let decision = crate::routing::decide(&routing_request, &benchmark, &pricing)?;
    s.workbench.teams.store.append_event(
        &id,
        "routing_preview",
        serde_json::to_value(&decision)?,
    )?;
    Ok(Json(serde_json::to_value(decision)?))
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
        .teams
        .store
        .events(&id, q.after, q.limit)?)))
}
async fn pricing(State(s): State<AppState>) -> Json<Value> {
    Json(s.workbench.pricing.status())
}
async fn refresh_pricing(State(s): State<AppState>) -> ApiResult {
    s.workbench.pricing.refresh().await?;
    Ok(Json(s.workbench.pricing.status()))
}
#[derive(Deserialize)]
struct QuoteQuery {
    app_id: String,
    model: String,
    billing_channel: String,
}
async fn quote(State(s): State<AppState>, Query(q): Query<QuoteQuery>) -> Json<Value> {
    Json(json!(s.workbench.pricing.quote(
        &q.app_id,
        &q.model,
        &q.billing_channel
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn review_rejections_are_not_successful_task_reports() {
        let rejected=parse_review(r#"{"accepted":false,"findings":["negative quantities accepted"],"summary":"missed requirement"}"#).unwrap();
        assert!(!rejected.accepted);
        assert!(parse_review("all good").is_err());
        assert!(parse_review(r#"{"accepted":true,"findings":[],"summary":""}"#).is_err());
    }
    fn request(dir: &Path) -> TeamCreate {
        TeamCreate {
            title: "Test collaboration".into(),
            prompt: "Implement and verify".into(),
            cwd: dir.to_string_lossy().into_owned(),
            strategy: TeamStrategy::Fixed,
            planner: ExecutorBinding {
                app_id: "kimi-cli".into(),
                model: "kimi-for-coding".into(),
                reasoning_effort: None,
            },
            candidates: vec![],
            nodes: vec![],
            checks: vec![crate::team_store::CheckSpec {
                program: "git".into(),
                args: vec!["--version".into()],
                timeout_secs: 5,
            }],
            max_parallel: 2,
            max_attempts: 2,
            max_duration_secs: 30,
            budget_usd: None,
        }
    }

    #[tokio::test]
    async fn team_children_persist_exact_bindings_and_retain_owner_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let mut children = Vec::new();
        {
            let service = WorkbenchService::open(&data).unwrap();
            for (app, model, effort) in [
                ("codex", "gpt-test", Some("high")),
                ("claude", "claude-sonnet-4-6", Some("xhigh")),
                ("deepseek", "deepseek-chat", Some("off")),
                ("kimi-cli", "kimi-for-coding", None),
            ] {
                let binding = ExecutorBinding {
                    app_id: app.into(),
                    model: model.into(),
                    reasoning_effort: effort.map(str::to_owned),
                };
                let mut input = request(dir.path());
                input.planner = binding.clone();
                let team = service.teams.create(input).unwrap();
                for read_only in [true, false] {
                    let child = create_child(
                        &service,
                        &team.id,
                        &binding,
                        dir.path(),
                        "bounded subtask".into(),
                        read_only,
                        Instant::now() + Duration::from_secs(30),
                        if read_only {
                            "Planner or review"
                        } else {
                            "Implementation"
                        },
                        vec![],
                    )
                    .unwrap();
                    assert_eq!(child.app_id, binding.app_id);
                    assert_eq!(child.model, binding.model);
                    assert_eq!(child.reasoning_effort, binding.reasoning_effort);
                    let events = service.store.events(&child.id, 0, 10).unwrap();
                    assert_eq!(
                        events
                            .iter()
                            .find(|event| event.kind == "team_owner")
                            .unwrap()
                            .data["executor"],
                        json!(binding)
                    );
                    children.push(service.store.get(&child.id).unwrap().unwrap());
                }
            }
        }
        let service = WorkbenchService::open(&data).unwrap();
        for child in children {
            assert_eq!(service.store.get(&child.id).unwrap(), Some(child.clone()));
            assert!(service
                .start(&child.id)
                .await
                .unwrap_err()
                .to_string()
                .contains("owned by a collaboration"));
            assert!(service
                .start_team_child(&child.id, "another-team")
                .await
                .unwrap_err()
                .to_string()
                .contains("another collaboration"));
            assert!(!service.children_running(&[child.id]).await);
        }
    }

    #[tokio::test]
    async fn child_cannot_start_with_a_different_persisted_reasoning_binding() {
        let dir = tempfile::tempdir().unwrap();
        let service = WorkbenchService::open(&dir.path().join("data")).unwrap();
        let child = service
            .create(WorkflowCreate {
                prompt: "test".into(),
                cwd: dir.path().to_string_lossy().into_owned(),
                app_id: "codex".into(),
                model: "gpt-test".into(),
                reasoning_effort: Some("high".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(service
            .start_team_child(&child.id, "team")
            .await
            .unwrap_err()
            .to_string()
            .contains("no collaboration owner"));
        service
            .store
            .append_event(
                &child.id,
                "team_owner",
                json!({
                    "team_id":"team", "executor":{
                        "app_id":"codex", "model":"gpt-test", "reasoning_effort":"low"
                    }
                }),
            )
            .unwrap();
        let before = service.store.events(&child.id, 0, 10).unwrap();
        let before_record = service.store.get(&child.id).unwrap();
        assert!(service
            .start_team_child(&child.id, "team")
            .await
            .unwrap_err()
            .to_string()
            .contains("do not match"));
        assert_eq!(service.store.get(&child.id).unwrap(), before_record);
        assert_eq!(service.store.events(&child.id, 0, 10).unwrap(), before);
        assert!(!service.children_running(&[child.id]).await);
    }

    #[tokio::test]
    async fn unknown_billing_blocks_before_any_native_child_or_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let service = WorkbenchService::open(&dir.path().join("data")).unwrap();
        let mut input = request(dir.path());
        input.budget_usd = Some(1.0);
        let team = service.teams.create(input).unwrap();
        TeamService::start(&service, &team.id).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while !service.teams.is_idle().await {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let result = service.teams.store.get(&team.id).unwrap().unwrap();
        assert_eq!(result.status, TeamStatus::Blocked);
        assert!(result.run_workspace.is_none());
        assert!(service.store.list().unwrap().is_empty());
        assert_eq!(
            service.teams.cancel(&team.id).await.unwrap().status,
            TeamStatus::Cancelled
        );
    }
    #[tokio::test]
    async fn permission_resume_preserves_other_waiting_children() {
        let dir = tempfile::tempdir().unwrap();
        let service = WorkbenchService::open(&dir.path().join("data")).unwrap();
        let team = service.teams.create(request(dir.path())).unwrap();
        service
            .teams
            .store
            .transition(&team.id, &[TeamStatus::Planned], TeamStatus::Running, None)
            .unwrap();
        service
            .teams
            .store
            .transition(
                &team.id,
                &[TeamStatus::Running],
                TeamStatus::WaitingInput,
                None,
            )
            .unwrap();
        let child = create_child(
            &service,
            &team.id,
            &team.request.planner,
            dir.path(),
            "test".into(),
            true,
            Instant::now() + Duration::from_secs(30),
            "planner",
            vec![],
        )
        .unwrap();
        service
            .store
            .transition(
                &child.id,
                &[WorkflowStatus::Draft],
                WorkflowStatus::Running,
                None,
            )
            .unwrap();
        service
            .store
            .transition(
                &child.id,
                &[WorkflowStatus::Running],
                WorkflowStatus::WaitingInput,
                None,
            )
            .unwrap();
        let (tx, _rx) = watch::channel(false);
        service.teams.jobs.lock().await.insert(
            team.id.clone(),
            TeamJob {
                cancel: tx,
                children: vec![child.id.clone()],
            },
        );
        resume_if_ready(&service, &team.id).await.unwrap();
        assert_eq!(
            service.teams.store.get(&team.id).unwrap().unwrap().status,
            TeamStatus::WaitingInput
        );
        service
            .store
            .transition(
                &child.id,
                &[WorkflowStatus::WaitingInput],
                WorkflowStatus::Verifying,
                None,
            )
            .unwrap();
        resume_if_ready(&service, &team.id).await.unwrap();
        assert_eq!(
            service.teams.store.get(&team.id).unwrap().unwrap().status,
            TeamStatus::Running
        );
        assert!(service.accept(&child.id, "user bypass").await.is_err());
        assert!(service.start(&child.id).await.is_err());
    }
    #[test]
    fn assigned_plan_requires_explicit_bindings_and_unsupported_tools_do_not_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let service = WorkbenchService::open(&dir.path().join("data")).unwrap();
        let mut input = request(dir.path());
        input.strategy = TeamStrategy::Assigned;
        input.candidates = vec![input.planner.clone()];
        assert!(service.teams.create(input).is_err());
        let mut input = request(dir.path());
        input.planner.app_id = "wonderland".into();
        assert!(service.teams.create(input).is_err());
    }
    #[test]
    fn strict_planner_output_rejects_prose_commands_and_cycles() {
        let valid = r#"{"nodes":[{"id":"a","objective":"Implement","dependencies":[],"write_paths":["src"],"acceptance":["passes"]}]}"#;
        assert_eq!(parse_plan(valid).unwrap().len(), 1);
        assert!(parse_plan(&format!("```json\n{valid}\n```")).is_ok());
        assert!(parse_plan(&format!("Here is your plan: {valid}")).is_err());
        assert!(
            parse_plan(&valid.replace("\"dependencies\":[]", "\"dependencies\":[\"a\"]")).is_err()
        );
        assert!(
            parse_plan(&valid.replace("\"id\":\"a\"", "\"id\":\"a\",\"command\":\"rm\"")).is_err()
        );
    }
}
