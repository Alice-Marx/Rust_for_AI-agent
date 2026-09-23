//! Durable team plans, attempts and budget reservations. This module never runs a worker.
//!
//! Call `recover_interrupted` only after acquiring exclusive service ownership.
//! All mutations are SQLite IMMEDIATE transactions; record revisions support optimistic
//! plan editing, and state transitions / attempt claims prevent duplicate execution.
use crate::routing::RoutingPolicy;
use anyhow::{ensure, Context, Result};
use chrono::{SecondsFormat, Utc};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{Mutex, MutexGuard},
    time::Duration,
};
use uuid::Uuid;

pub const MAX_NODES: usize = 24;
pub const MAX_ATTEMPTS: u32 = 5;
pub const MAX_BUDGET_USD: f64 = 100_000.0;
const MAX_EVENT_BYTES: usize = 262_144;
const MAX_RECORD_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutorBinding {
    pub app_id: String,
    pub model: String,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TeamStrategy {
    #[default]
    Fixed,
    Assigned,
    Automatic,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NodeSpec {
    pub id: String,
    pub objective: String,
    #[serde(default)]
    pub dependencies: Vec<String>,
    pub write_paths: Vec<String>,
    pub acceptance: Vec<String>,
    #[serde(default)]
    pub executor: Option<ExecutorBinding>,
}

/// Explicit program and argv. No shell parsing or interpolation is performed.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckSpec {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub timeout_secs: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TeamCreate {
    #[serde(default)]
    pub title: String,
    pub prompt: String,
    pub cwd: String,
    #[serde(default)]
    pub strategy: TeamStrategy,
    pub planner: ExecutorBinding,
    #[serde(default)]
    pub candidates: Vec<ExecutorBinding>,
    #[serde(default)]
    pub nodes: Vec<NodeSpec>,
    pub checks: Vec<CheckSpec>,
    #[serde(default = "default_parallel")]
    pub max_parallel: usize,
    #[serde(default = "default_duration")]
    pub max_duration_secs: u64,
    #[serde(default = "default_attempts")]
    pub max_attempts: u32,
    #[serde(default)]
    pub budget_usd: Option<f64>,
    /// Optional saved constraints for reproducible Automatic routing previews.
    #[serde(default)]
    pub routing_policy: Option<RoutingPolicy>,
}
fn default_parallel() -> usize {
    2
}
fn default_duration() -> u64 {
    3600
}
fn default_attempts() -> u32 {
    2
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TeamStatus {
    Planned,
    Running,
    WaitingInput,
    Verifying,
    Succeeded,
    Failed,
    Cancelled,
    Blocked,
    Interrupted,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Pending,
    Running,
    WaitingInput,
    Verifying,
    Succeeded,
    Failed,
    Cancelled,
    Blocked,
    Interrupted,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReservationStatus {
    Reserved,
    Settled,
    Released,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BudgetReservation {
    pub id: String,
    pub node_id: String,
    pub attempt: u32,
    /// Integer millionths of a USD; reservation rounds up and budget rounds down.
    pub reserved_microusd: u64,
    pub actual_microusd: Option<u64>,
    pub status: ReservationStatus,
    pub created_at: String,
    pub settled_at: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct NodeAttempt {
    pub number: u32,
    pub executor: ExecutorBinding,
    pub status: NodeStatus,
    pub workflow_id: Option<String>,
    pub workspace: Option<String>,
    pub reservation_id: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub error: Option<String>,
    pub artifact: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct NodeRecord {
    pub spec: NodeSpec,
    pub status: NodeStatus,
    pub attempts: Vec<NodeAttempt>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckResult {
    pub check_index: usize,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub output: String,
    pub duration_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TeamRecord {
    pub id: String,
    pub request: TeamCreate,
    pub status: TeamStatus,
    pub revision: u64,
    pub created_at: String,
    pub updated_at: String,
    pub error: Option<String>,
    pub starting_revision: Option<String>,
    pub run_workspace: Option<String>,
    pub nodes: Vec<NodeRecord>,
    pub reservations: Vec<BudgetReservation>,
    pub verification: Vec<CheckResult>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TeamEvent {
    pub seq: i64,
    pub team_id: String,
    pub kind: String,
    pub data: Value,
    pub created_at: String,
}

pub struct TeamStore {
    connection: Mutex<Connection>,
}

impl TeamStatus {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Running | Self::WaitingInput | Self::Verifying)
    }
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Running => "running",
            Self::WaitingInput => "waiting_input",
            Self::Verifying => "verifying",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Blocked => "blocked",
            Self::Interrupted => "interrupted",
        }
    }
    fn permits(self, next: Self) -> bool {
        use TeamStatus::*;
        match self {
            Planned => matches!(next, Running | Blocked | Cancelled),
            Running => matches!(
                next,
                WaitingInput | Verifying | Failed | Cancelled | Blocked | Interrupted
            ),
            WaitingInput => matches!(next, Running | Failed | Cancelled | Blocked | Interrupted),
            Verifying => matches!(next, Succeeded | Failed | Cancelled | Blocked | Interrupted),
            Blocked => matches!(next, Running | Failed | Cancelled),
            // A failed or interrupted run is preserved; retry by creating a new run.
            Succeeded | Failed | Cancelled | Interrupted => false,
        }
    }
}
impl NodeStatus {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Running | Self::WaitingInput | Self::Verifying)
    }
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Blocked | Self::Interrupted
        )
    }
    fn permits(self, next: Self) -> bool {
        use NodeStatus::*;
        match self {
            Pending => false, // begin_attempt is the only execution claim.
            Running => matches!(
                next,
                WaitingInput | Verifying | Failed | Cancelled | Blocked | Interrupted
            ),
            WaitingInput => matches!(
                next,
                Running | Verifying | Failed | Cancelled | Blocked | Interrupted
            ),
            Verifying => matches!(next, Succeeded | Failed | Cancelled | Blocked | Interrupted),
            Succeeded | Failed | Cancelled | Blocked | Interrupted => false,
        }
    }
}

/// Syntactic validation only. The service separately validates available adapters and models.
pub fn validate_create(request: &TeamCreate) -> Result<()> {
    bounded(&request.title, 512, "title", true)?;
    bounded(&request.prompt, 262_144, "prompt", false)?;
    bounded(&request.cwd, 8192, "cwd", false)?;
    ensure!(
        Path::new(&request.cwd).is_absolute(),
        "team cwd must be an absolute directory"
    );
    validate_executor(&request.planner)?;
    ensure!(
        request.candidates.len() <= 24,
        "at most 24 executor candidates are allowed"
    );
    let mut bindings = HashSet::new();
    for candidate in &request.candidates {
        validate_executor(candidate)?;
        ensure!(
            bindings.insert(serde_json::to_string(candidate)?),
            "duplicate executor candidate"
        );
    }
    if matches!(
        request.strategy,
        TeamStrategy::Automatic | TeamStrategy::Assigned
    ) {
        ensure!(
            !request.candidates.is_empty(),
            "assigned or automatic routing requires explicit executor candidates"
        );
    }
    if request.routing_policy.is_some() {
        ensure!(
            request.strategy == TeamStrategy::Automatic,
            "routing_policy is only valid for automatic teams"
        );
    }
    ensure!(
        (1..=8).contains(&request.max_parallel),
        "max_parallel must be between 1 and 8"
    );
    ensure!(
        (1..=86_400).contains(&request.max_duration_secs),
        "max_duration_secs must be between 1 and 86400"
    );
    ensure!(
        (1..=MAX_ATTEMPTS).contains(&request.max_attempts),
        "max_attempts must be between 1 and {MAX_ATTEMPTS}"
    );
    if let Some(budget) = request.budget_usd {
        money(budget, false)?;
        ensure!(budget >= 0.000001, "budget must be at least one microUSD");
    }
    ensure!(
        !request.checks.is_empty() && request.checks.len() <= 32,
        "1 to 32 explicit acceptance commands are required"
    );
    for check in &request.checks {
        validate_check(check)?;
        ensure!(
            check.timeout_secs <= request.max_duration_secs,
            "check timeout exceeds run duration"
        );
    }
    if !request.nodes.is_empty() {
        validate_plan(&request.nodes)?;
        validate_plan_bindings(request, &request.nodes)?;
    }
    Ok(())
}

pub fn validate_executor(binding: &ExecutorBinding) -> Result<()> {
    identifier(&binding.app_id, "app_id", 64)?;
    bounded(&binding.model, 256, "model", false)?;
    ensure!(
        binding.model == binding.model.trim() && !binding.model.chars().any(char::is_control),
        "invalid model identifier"
    );
    if let Some(effort) = &binding.reasoning_effort {
        identifier(effort, "reasoning_effort", 32)?;
    }
    Ok(())
}

pub fn validate_check(check: &CheckSpec) -> Result<()> {
    bounded(&check.program, 8192, "check program", false)?;
    ensure!(
        check.program == check.program.trim() && !check.program.chars().any(char::is_control),
        "invalid check program"
    );
    let name = check
        .program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    ensure!(
        !matches!(
            name.trim_end_matches(".exe"),
            "cmd" | "powershell" | "pwsh" | "sh" | "bash" | "zsh" | "fish" | "csh" | "dash"
        ),
        "checks require an executable and argv, not a shell command"
    );
    ensure!(check.args.len() <= 128, "too many check arguments");
    let mut bytes = check.program.len();
    for arg in &check.args {
        bounded(arg, 16_384, "check argument", true)?;
        bytes += arg.len();
    }
    ensure!(bytes <= 65_536, "acceptance command is too large");
    ensure!(
        (1..=3600).contains(&check.timeout_secs),
        "check timeout must be between 1 and 3600 seconds"
    );
    Ok(())
}

/// Literal, portable project-relative file/directory prefixes, not glob expressions.
pub fn normalize_write_path(path: &str) -> Result<String> {
    bounded(path, 4096, "write path", false)?;
    ensure!(
        !path
            .chars()
            .any(|c| c.is_control()
                || matches!(c, ':' | '*' | '?' | '[' | ']' | '\"' | '<' | '>' | '|')),
        "invalid write path"
    );
    let normalized = path.replace('\\', "/");
    ensure!(
        !normalized.starts_with('/') && !normalized.ends_with('/'),
        "write path must be project-relative without trailing separators"
    );
    for part in normalized.split('/') {
        ensure!(
            !part.is_empty() && part != "." && part != "..",
            "write path contains traversal or empty components"
        );
        ensure!(
            !part.eq_ignore_ascii_case(".git"),
            "the Git control directory is never a write scope"
        );
        ensure!(
            !part.ends_with(['.', ' ']),
            "write path has ambiguous Windows suffix"
        );
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        ensure!(
            !matches!(
                stem.as_str(),
                "CON"
                    | "PRN"
                    | "AUX"
                    | "NUL"
                    | "COM1"
                    | "COM2"
                    | "COM3"
                    | "COM4"
                    | "COM5"
                    | "COM6"
                    | "COM7"
                    | "COM8"
                    | "COM9"
                    | "LPT1"
                    | "LPT2"
                    | "LPT3"
                    | "LPT4"
                    | "LPT5"
                    | "LPT6"
                    | "LPT7"
                    | "LPT8"
                    | "LPT9"
            ),
            "reserved device filename is not a write scope"
        );
    }
    Ok(normalized)
}

pub fn validate_plan(nodes: &[NodeSpec]) -> Result<()> {
    ensure!(
        !nodes.is_empty() && nodes.len() <= MAX_NODES,
        "plan must contain 1 to {MAX_NODES} nodes"
    );
    let mut ids = HashMap::new();
    for (index, node) in nodes.iter().enumerate() {
        identifier(&node.id, "node id", 64)?;
        ensure!(
            ids.insert(node.id.clone(), index).is_none(),
            "duplicate node id: {}",
            node.id
        );
        bounded(&node.objective, 65_536, "node objective", false)?;
        ensure!(node.dependencies.len() < MAX_NODES, "too many dependencies");
        ensure!(node.write_paths.len() <= 128, "too many write paths");
        let mut paths = HashSet::new();
        for path in &node.write_paths {
            ensure!(
                paths.insert(normalize_write_path(path)?.to_ascii_lowercase()),
                "duplicate write scope"
            );
        }
        ensure!(
            !node.acceptance.is_empty() && node.acceptance.len() <= 32,
            "each node requires 1 to 32 acceptance criteria"
        );
        for criterion in &node.acceptance {
            bounded(criterion, 4096, "acceptance criterion", false)?;
        }
        if let Some(binding) = &node.executor {
            validate_executor(binding)?;
        }
    }
    let mut degree = vec![0; nodes.len()];
    let mut outgoing = vec![vec![]; nodes.len()];
    for (index, node) in nodes.iter().enumerate() {
        let mut dependencies = HashSet::new();
        for dependency in &node.dependencies {
            let &parent = ids
                .get(dependency)
                .with_context(|| format!("unknown dependency {dependency}"))?;
            ensure!(parent != index, "node cannot depend on itself");
            ensure!(dependencies.insert(dependency), "duplicate dependency");
            degree[index] += 1;
            outgoing[parent].push(index);
        }
    }
    let mut queue: Vec<usize> = degree
        .iter()
        .enumerate()
        .filter_map(|(i, d)| (*d == 0).then_some(i))
        .collect();
    let mut visited = 0;
    while let Some(index) = queue.pop() {
        visited += 1;
        for &child in &outgoing[index] {
            degree[child] -= 1;
            if degree[child] == 0 {
                queue.push(child);
            }
        }
    }
    ensure!(
        visited == nodes.len(),
        "plan dependency graph contains a cycle"
    );
    Ok(())
}

fn validate_plan_bindings(request: &TeamCreate, nodes: &[NodeSpec]) -> Result<()> {
    for node in nodes {
        if request.strategy == TeamStrategy::Assigned {
            ensure!(
                node.executor.is_some(),
                "assigned strategy requires an executor on every node"
            );
        }
        if let Some(binding) = &node.executor {
            validate_binding_choice(request, binding)?;
        }
    }
    Ok(())
}
fn validate_binding_choice(request: &TeamCreate, binding: &ExecutorBinding) -> Result<()> {
    validate_executor(binding)?;
    match request.strategy {
        TeamStrategy::Fixed => ensure!(
            *binding == request.planner,
            "fixed strategy cannot switch the pinned executor"
        ),
        TeamStrategy::Automatic | TeamStrategy::Assigned => ensure!(
            request.candidates.contains(binding),
            "executor is not in the allowed candidate list"
        ),
    };
    Ok(())
}
fn bounded(value: &str, max: usize, name: &str, empty: bool) -> Result<()> {
    ensure!(
        value.len() <= max && !value.contains('\0') && (empty || !value.trim().is_empty()),
        "invalid or oversized {name}"
    );
    Ok(())
}
fn identifier(value: &str, name: &str, max: usize) -> Result<()> {
    bounded(value, max, name, false)?;
    ensure!(
        value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            && !matches!(value, "." | ".."),
        "invalid {name}"
    );
    Ok(())
}
fn money(usd: f64, round_up: bool) -> Result<u64> {
    ensure!(
        usd.is_finite() && usd >= 0.0 && usd <= MAX_BUDGET_USD,
        "cost must be finite and between 0 and {MAX_BUDGET_USD} USD"
    );
    let scaled = usd * 1_000_000.0;
    Ok(if round_up {
        scaled.ceil()
    } else {
        scaled.floor()
    } as u64)
}
fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

impl TeamStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        Self::from_connection(Connection::open(path)?)
    }
    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }
    fn from_connection(mut connection: Connection) -> Result<Self> {
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;",
        )?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS team_schema(version INTEGER NOT NULL); INSERT INTO team_schema(version) SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM team_schema);")?;
        let version: i64 = tx.query_row("SELECT version FROM team_schema", [], |r| r.get(0))?;
        ensure!(version == 1, "unsupported team database schema {version}");
        tx.execute_batch("CREATE TABLE IF NOT EXISTS teams(id TEXT PRIMARY KEY NOT NULL,status TEXT NOT NULL,revision INTEGER NOT NULL,updated_at TEXT NOT NULL,record TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS teams_updated ON teams(updated_at DESC);
            CREATE TABLE IF NOT EXISTS team_events(seq INTEGER PRIMARY KEY AUTOINCREMENT,team_id TEXT NOT NULL REFERENCES teams(id) ON DELETE RESTRICT,kind TEXT NOT NULL,data TEXT NOT NULL,created_at TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS team_events_cursor ON team_events(team_id,seq);
            CREATE TABLE IF NOT EXISTS team_child_workflows(workflow_id TEXT PRIMARY KEY NOT NULL,team_id TEXT NOT NULL REFERENCES teams(id) ON DELETE RESTRICT,node_id TEXT NOT NULL,attempt INTEGER NOT NULL,UNIQUE(team_id,node_id,attempt));")?;
        tx.commit()?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }
    fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| anyhow::anyhow!("team database lock poisoned"))
    }
    pub fn create(&self, mut request: TeamCreate) -> Result<TeamRecord> {
        validate_create(&request)?;
        normalize_nodes(&mut request.nodes)?;
        if request.title.trim().is_empty() {
            request.title = request.prompt.chars().take(100).collect();
        }
        let timestamp = now();
        let record = TeamRecord {
            id: format!("team_{}", Uuid::new_v4().simple()),
            nodes: request
                .nodes
                .iter()
                .cloned()
                .map(|spec| NodeRecord {
                    spec,
                    status: NodeStatus::Pending,
                    attempts: vec![],
                    error: None,
                })
                .collect(),
            request,
            status: TeamStatus::Planned,
            revision: 1,
            created_at: timestamp.clone(),
            updated_at: timestamp,
            error: None,
            starting_revision: None,
            run_workspace: None,
            reservations: vec![],
            verification: vec![],
        };
        let encoded = encode_record(&record)?;
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO teams(id,status,revision,updated_at,record) VALUES(?1,?2,?3,?4,?5)",
            params![
                record.id,
                record.status.as_str(),
                i64::try_from(record.revision)?,
                record.updated_at,
                encoded
            ],
        )?;
        insert_event(
            &tx,
            &record.id,
            "created",
            json!({"strategy":record.request.strategy,"nodes":record.nodes.len()}),
        )?;
        tx.commit()?;
        Ok(record)
    }
    pub fn get(&self, id: &str) -> Result<Option<TeamRecord>> {
        read_record(&*self.lock()?, id)
    }
    pub fn list(&self) -> Result<Vec<TeamRecord>> {
        let conn = self.lock()?;
        let mut statement = conn.prepare("SELECT record FROM teams ORDER BY updated_at DESC,id")?;
        let encoded = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        encoded.into_iter().map(|s| decode_record(&s)).collect()
    }
    fn mutate<F>(&self, id: &str, kind: &str, operation: F) -> Result<TeamRecord>
    where
        F: FnOnce(&Transaction<'_>, &mut TeamRecord) -> Result<Value>,
    {
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut record = read_record(&tx, id)?.context("team not found")?;
        let old_revision = record.revision;
        let data = operation(&tx, &mut record)?;
        record.revision = old_revision
            .checked_add(1)
            .context("team revision exhausted")?;
        record.updated_at = now();
        let encoded = encode_record(&record)?;
        ensure!(tx.execute("UPDATE teams SET status=?1,revision=?2,updated_at=?3,record=?4 WHERE id=?5 AND revision=?6",params![record.status.as_str(),i64::try_from(record.revision)?,record.updated_at,encoded,id,i64::try_from(old_revision)?])?==1,"team changed concurrently");
        insert_event(&tx, id, kind, data)?;
        tx.commit()?;
        Ok(record)
    }
    pub fn append_event(&self, id: &str, kind: &str, data: Value) -> Result<TeamEvent> {
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(read_record(&tx, id)?.is_some(), "team not found");
        let event = insert_event(&tx, id, kind, data)?;
        tx.commit()?;
        Ok(event)
    }
    pub fn events(&self, id: &str, after: i64, limit: usize) -> Result<Vec<TeamEvent>> {
        ensure!(after >= 0, "event cursor must not be negative");
        ensure!(
            (1..=1000).contains(&limit),
            "event page limit must be between 1 and 1000"
        );
        let conn = self.lock()?;
        let mut statement=conn.prepare("SELECT seq,team_id,kind,data,created_at FROM team_events WHERE team_id=?1 AND seq>?2 ORDER BY seq LIMIT ?3")?;
        let rows = statement
            .query_map(params![id, after, limit as i64], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(seq, team_id, kind, data, created_at)| {
                Ok(TeamEvent {
                    seq,
                    team_id,
                    kind,
                    data: serde_json::from_str(&data)?,
                    created_at,
                })
            })
            .collect()
    }
    pub fn latest_event(&self, id: &str, kind: &str) -> Result<Option<TeamEvent>> {
        identifier(kind, "event kind", 80)?;
        let conn = self.lock()?;
        ensure!(read_record(&conn, id)?.is_some(), "team not found");
        let row = conn
            .query_row(
                "SELECT seq,team_id,kind,data,created_at FROM team_events WHERE team_id=?1 AND kind=?2 ORDER BY seq DESC LIMIT 1",
                params![id, kind],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(seq, team_id, kind, data, created_at)| {
            Ok(TeamEvent {
                seq,
                team_id,
                kind,
                data: serde_json::from_str(&data)?,
                created_at,
            })
        })
        .transpose()
    }
    pub fn transition(
        &self,
        id: &str,
        expected: &[TeamStatus],
        next: TeamStatus,
        error: Option<&str>,
    ) -> Result<TeamRecord> {
        validate_error(error)?;
        self.mutate(id, "status", |_, record| {
            ensure!(
                expected.contains(&record.status),
                "team status changed; expected {expected:?}, found {:?}",
                record.status
            );
            ensure!(
                record.status.permits(next),
                "invalid team transition {:?} -> {next:?}",
                record.status
            );
            if next == TeamStatus::Verifying {
                ensure!(
                    !record.nodes.is_empty()
                        && record
                            .nodes
                            .iter()
                            .all(|n| n.status == NodeStatus::Succeeded),
                    "all nodes must succeed before aggregate verification"
                );
            }
            if next == TeamStatus::Succeeded {
                validate_passed_verification(record)?;
            }
            let previous = record.status;
            record.status = next;
            record.error = error.map(str::to_owned);
            if matches!(
                next,
                TeamStatus::Cancelled | TeamStatus::Failed | TeamStatus::Interrupted
            ) {
                // Children still need process cancellation by the service; recording the
                // terminal outcome here prevents any new attempts or late success reports.
                let node_status = match next {
                    TeamStatus::Cancelled => NodeStatus::Cancelled,
                    TeamStatus::Interrupted => NodeStatus::Interrupted,
                    _ => NodeStatus::Failed,
                };
                stop_active_attempts(record, node_status, error);
            }
            Ok(json!({"from":previous,"to":next,"error":error}))
        })
    }
    pub fn set_plan(
        &self,
        id: &str,
        expected_revision: u64,
        mut nodes: Vec<NodeSpec>,
    ) -> Result<TeamRecord> {
        validate_plan(&nodes)?;
        normalize_nodes(&mut nodes)?;
        self.mutate(id, "plan_set", |_, record| {
            ensure!(
                record.revision == expected_revision,
                "plan revision changed"
            );
            ensure!(
                matches!(record.status, TeamStatus::Planned | TeamStatus::Running),
                "plan is not editable in this state"
            );
            ensure!(
                record.nodes.iter().all(|n| n.attempts.is_empty()),
                "plan is immutable after execution begins"
            );
            validate_plan_bindings(&record.request, &nodes)?;
            record.request.nodes = nodes.clone();
            record.nodes = nodes
                .into_iter()
                .map(|spec| NodeRecord {
                    spec,
                    status: NodeStatus::Pending,
                    attempts: vec![],
                    error: None,
                })
                .collect();
            record.verification.clear();
            Ok(json!({"nodes":record.nodes.iter().map(|n|n.spec.id.as_str()).collect::<Vec<_>>()}))
        })
    }
    /// Persist a routing-selected binding as the planner (and therefore the
    /// default executor for every node without an explicit binding). Only
    /// legal while no attempt has begun, and only for a declared candidate.
    pub fn set_planner(
        &self,
        id: &str,
        expected_revision: u64,
        binding: ExecutorBinding,
    ) -> Result<TeamRecord> {
        crate::native_executor::validate_binding(
            &binding.app_id,
            &binding.model,
            binding.reasoning_effort.as_deref(),
            false,
        )?;
        self.mutate(id, "planner_set", move |_, record| {
            ensure!(
                record.revision == expected_revision,
                "planner revision changed"
            );
            ensure!(
                record.status == TeamStatus::Running,
                "a routing binding can only be applied while the team is starting"
            );
            ensure!(
                record.nodes.iter().all(|n| n.attempts.is_empty()),
                "the routing binding is immutable after execution begins"
            );
            ensure!(
                record
                    .request
                    .candidates
                    .iter()
                    .any(|candidate| candidate == &binding),
                "routing selected a binding that the team did not declare"
            );
            record.request.planner = binding.clone();
            Ok(json!({"planner": binding}))
        })
    }
    pub fn set_workspace(
        &self,
        id: &str,
        starting_revision: &str,
        run_workspace: &str,
    ) -> Result<TeamRecord> {
        ensure!(
            matches!(starting_revision.len(), 40 | 64)
                && starting_revision.bytes().all(|b| b.is_ascii_hexdigit()),
            "starting revision must be a full Git object ID"
        );
        bounded(run_workspace, 8192, "run workspace", false)?;
        ensure!(
            Path::new(run_workspace).is_absolute(),
            "run workspace must be absolute"
        );
        self.mutate(id, "workspace_set", |_, record| {
            ensure!(
                matches!(record.status, TeamStatus::Planned | TeamStatus::Running),
                "workspace cannot be assigned in this state"
            );
            ensure!(
                record.run_workspace.is_none()
                    && record.starting_revision.is_none()
                    && record.nodes.iter().all(|n| n.attempts.is_empty()),
                "run workspace is already pinned"
            );
            ensure!(
                !same_path(&record.request.cwd, run_workspace),
                "run workspace must not be the user's checkout"
            );
            record.starting_revision = Some(starting_revision.into());
            record.run_workspace = Some(run_workspace.into());
            Ok(json!({"revision":starting_revision,"workspace":run_workspace}))
        })
    }
    pub fn begin_attempt(
        &self,
        id: &str,
        node_id: &str,
        executor: ExecutorBinding,
        estimated_cost_usd: Option<f64>,
    ) -> Result<NodeAttempt> {
        validate_executor(&executor)?;
        let estimate = estimated_cost_usd
            .map(|cost| money(cost, true))
            .transpose()?;
        let result = self.mutate(id, "attempt_started", |_, record| {
            ensure!(record.status == TeamStatus::Running, "team is not running");
            ensure!(
                record.run_workspace.is_some() && record.starting_revision.is_some(),
                "isolated run workspace must be pinned before execution"
            );
            validate_binding_choice(&record.request, &executor)?;
            let index = record
                .nodes
                .iter()
                .position(|n| n.spec.id == node_id)
                .context("node not found")?;
            let node = &record.nodes[index];
            if let Some(binding) = &node.spec.executor {
                ensure!(*binding == executor, "node executor is pinned by its plan");
            }
            ensure!(
                matches!(
                    node.status,
                    NodeStatus::Pending
                        | NodeStatus::Failed
                        | NodeStatus::Blocked
                        | NodeStatus::Interrupted
                ),
                "node already active or complete"
            );
            ensure!(
                node.attempts.len() < (record.request.max_attempts as usize),
                "node attempt limit reached"
            );
            ensure!(
                node.spec.dependencies.iter().all(|id| record
                    .nodes
                    .iter()
                    .any(|n| n.spec.id == *id && n.status == NodeStatus::Succeeded)),
                "node dependencies have not succeeded"
            );
            ensure!(
                record.nodes.iter().filter(|n| n.status.is_active()).count()
                    < record.request.max_parallel,
                "parallel execution limit reached"
            );
            for active in record.nodes.iter().filter(|n| n.status.is_active()) {
                ensure!(
                    !scopes_overlap(&active.spec.write_paths, &node.spec.write_paths),
                    "a running node reserves an overlapping write scope"
                );
            }
            let number = node.attempts.len() as u32 + 1;
            let budget = record
                .request
                .budget_usd
                .map(|v| money(v, false))
                .transpose()?;
            if let Some(budget) = budget {
                let estimate = estimate
                    .filter(|n| *n > 0)
                    .context("a known positive cost reservation is required for a budgeted run")?;
                ensure!(
                    charged_microusd(record)
                        .checked_add(estimate)
                        .is_some_and(|sum| sum <= budget),
                    "team budget exhausted"
                );
            }
            let reservation_id = if let Some(amount) = estimate {
                let reservation = BudgetReservation {
                    id: Uuid::new_v4().to_string(),
                    node_id: node_id.into(),
                    attempt: number,
                    reserved_microusd: amount,
                    actual_microusd: None,
                    status: ReservationStatus::Reserved,
                    created_at: now(),
                    settled_at: None,
                };
                let key = reservation.id.clone();
                record.reservations.push(reservation);
                Some(key)
            } else {
                None
            };
            let attempt = NodeAttempt {
                number,
                executor,
                status: NodeStatus::Running,
                workflow_id: None,
                workspace: None,
                reservation_id,
                started_at: now(),
                finished_at: None,
                error: None,
                artifact: None,
            };
            record.nodes[index].status = NodeStatus::Running;
            record.nodes[index].error = None;
            record.nodes[index].attempts.push(attempt.clone());
            record.verification.clear();
            Ok(json!({"node_id":node_id,"attempt":attempt}))
        })?;
        Ok(result
            .nodes
            .iter()
            .find(|n| n.spec.id == node_id)
            .unwrap()
            .attempts
            .last()
            .unwrap()
            .clone())
    }
    pub fn attach_child(
        &self,
        id: &str,
        node_id: &str,
        number: u32,
        workflow_id: &str,
        workspace: &str,
    ) -> Result<TeamRecord> {
        identifier(workflow_id, "child workflow id", 128)?;
        bounded(workspace, 8192, "child workspace", false)?;
        ensure!(
            Path::new(workspace).is_absolute(),
            "child workspace must be absolute"
        );
        self.mutate(id,"child_attached",|tx,record|{
            ensure!(record.status.is_active(),"team is no longer active");ensure!(!same_path(workspace,&record.request.cwd),"child must not execute in the user's checkout");
            let attempt=latest_attempt_mut(record,node_id,number)?;
            ensure!(attempt.status==NodeStatus::Running&&attempt.workflow_id.is_none(),"attempt already has a child or has stopped");
            tx.execute("INSERT INTO team_child_workflows(workflow_id,team_id,node_id,attempt) VALUES(?1,?2,?3,?4)",params![workflow_id,id,node_id,number]).context("child workflow is already owned by an attempt")?;
            attempt.workflow_id=Some(workflow_id.into());attempt.workspace=Some(workspace.into());
            Ok(json!({"node_id":node_id,"attempt":number,"workflow_id":workflow_id,"workspace":workspace}))
        })
    }
    pub fn set_attempt_status(
        &self,
        id: &str,
        node_id: &str,
        number: u32,
        next: NodeStatus,
        error: Option<&str>,
        artifact: Option<Value>,
    ) -> Result<TeamRecord> {
        validate_error(error)?;
        if let Some(value) = &artifact {
            ensure!(
                serde_json::to_vec(value)?.len() <= 65_536,
                "attempt artifact too large"
            );
        }
        self.mutate(id, "attempt_status", |_, record| {
            ensure!(
                record.status.is_active() || record.status == TeamStatus::Blocked,
                "team is no longer active"
            );
            let attempt = latest_attempt_mut(record, node_id, number)?;
            ensure!(
                attempt.status.permits(next),
                "invalid attempt transition {:?} -> {next:?}",
                attempt.status
            );
            if next == NodeStatus::Succeeded {
                ensure!(
                    attempt.workflow_id.is_some(),
                    "successful attempt must have an attached child workflow"
                );
                ensure!(
                    artifact
                        .as_ref()
                        .is_some_and(|v| v.as_object().is_some_and(|o| !o.is_empty())),
                    "successful attempt needs verification artifact"
                );
            }
            let previous = attempt.status;
            attempt.status = next;
            attempt.error = error.map(str::to_owned);
            if artifact.is_some() {
                attempt.artifact = artifact;
            }
            if next.is_terminal() {
                attempt.finished_at = Some(now());
            }
            let node = record
                .nodes
                .iter_mut()
                .find(|n| n.spec.id == node_id)
                .unwrap();
            node.status = next;
            node.error = error.map(str::to_owned);
            Ok(json!({"node_id":node_id,"attempt":number,"from":previous,"to":next,"error":error}))
        })
    }
    pub fn settle_reservation(
        &self,
        id: &str,
        reservation_id: &str,
        actual_cost_usd: Option<f64>,
    ) -> Result<TeamRecord> {
        let actual = actual_cost_usd.map(|v| money(v, true)).transpose()?;
        self.mutate(id,"budget_settled",|_,record|{
            let reservation=record.reservations.iter().find(|r|r.id==reservation_id).context("reservation not found")?;
            let attempt=record.nodes.iter().find(|n|n.spec.id==reservation.node_id).and_then(|n|n.attempts.iter().find(|a|a.number==reservation.attempt)).context("reservation attempt missing")?;
            ensure!(attempt.status.is_terminal(),"active attempt cost cannot be finalized");
            let reservation=record.reservations.iter_mut().find(|r|r.id==reservation_id).context("reservation not found")?;
            ensure!(reservation.status==ReservationStatus::Reserved,"reservation already settled or released");
            reservation.actual_microusd=actual;reservation.status=ReservationStatus::Settled;reservation.settled_at=Some(now());
            let charged=charged_microusd(record);
            if let Some(budget)=record.request.budget_usd {if charged>money(budget,false)?&&record.status.is_active(){record.status=TeamStatus::Blocked;record.error=Some("Actual recorded cost exceeded the team budget; no further attempts may start".into());}}
            Ok(json!({"reservation_id":reservation_id,"actual_cost_usd":actual_cost_usd,"known":actual.is_some(),"charged_microusd":charged}))
        })
    }
    /// Settle the reservation attached to one attempt, if any. Actual cost
    /// stays None (conservative: charged falls back to the reserved amount)
    /// until a provider-confirmed invoice source exists.
    pub fn settle_attempt_reservation(&self, id: &str, node_id: &str, number: u32) -> Result<()> {
        let reservation_id = self
            .get(id)?
            .context("team not found")?
            .nodes
            .iter()
            .find(|n| n.spec.id == node_id)
            .and_then(|n| n.attempts.iter().find(|a| a.number == number))
            .and_then(|a| a.reservation_id.clone());
        if let Some(reservation_id) = reservation_id {
            self.settle_reservation(id, &reservation_id, None)?;
        }
        Ok(())
    }

    /// Close every reservation still marked Reserved on a team: attempts that
    /// reached a terminal state settle conservatively at their reserved amount
    /// (no provider-confirmed actual exists yet); attempts that never launched
    /// are released. Called when a run finalizes, before a Blocked team is
    /// retried, and after service restarts so stale holds never leak.
    pub fn reconcile_reservations(&self, id: &str, reason: &str) -> Result<TeamRecord> {
        bounded(reason, 256, "reconciliation reason", false)?;
        self.mutate(id, "budget_reconciled", |_, record| {
            let mut reconciled = Vec::new();
            for reservation in &mut record.reservations {
                if reservation.status != ReservationStatus::Reserved {
                    continue;
                }
                let attempt_ended = record.nodes.iter().any(|n| {
                    n.spec.id == reservation.node_id
                        && n.attempts
                            .iter()
                            .any(|a| a.number == reservation.attempt && a.status.is_terminal())
                });
                let outcome = if attempt_ended {
                    "settled_at_reserved"
                } else {
                    "released"
                };
                reservation.status = if attempt_ended {
                    ReservationStatus::Settled
                } else {
                    ReservationStatus::Released
                };
                reservation.settled_at = Some(now());
                reconciled.push(json!({
                    "reservation_id": reservation.id,
                    "node_id": reservation.node_id,
                    "attempt": reservation.attempt,
                    "outcome": outcome,
                }));
            }
            Ok(json!({"reason": reason, "reservations": reconciled}))
        })
    }
    pub fn release_reservation(&self, id: &str, reservation_id: &str) -> Result<TeamRecord> {
        self.mutate(id, "budget_released", |_, record| {
            let reservation = record
                .reservations
                .iter()
                .find(|r| r.id == reservation_id)
                .context("reservation not found")?;
            ensure!(
                reservation.status == ReservationStatus::Reserved,
                "reservation already settled or released"
            );
            let attempt = record
                .nodes
                .iter()
                .find(|n| n.spec.id == reservation.node_id)
                .and_then(|n| n.attempts.iter().find(|a| a.number == reservation.attempt))
                .context("reservation attempt missing")?;
            ensure!(
                attempt.workflow_id.is_none() && attempt.status.is_terminal(),
                "only an unlaunched, ended attempt can release its reservation"
            );
            let reservation = record
                .reservations
                .iter_mut()
                .find(|r| r.id == reservation_id)
                .unwrap();
            reservation.status = ReservationStatus::Released;
            reservation.settled_at = Some(now());
            Ok(json!({"reservation_id":reservation_id}))
        })
    }
    pub fn record_verification(&self, id: &str, results: Vec<CheckResult>) -> Result<TeamRecord> {
        self.mutate(id, "verification", |_, record| {
            ensure!(
                record.status == TeamStatus::Verifying,
                "team is not awaiting verification"
            );
            validate_check_results(&record.request.checks, &results)?;
            record.verification = results;
            Ok(json!({"checks":record.verification.iter().map(|r|json!({"check_index":r.check_index,"success":r.success,"exit_code":r.exit_code,"timed_out":r.timed_out})).collect::<Vec<_>>()}))
        })
    }
    pub fn recover_interrupted(&self) -> Result<usize> {
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let encoded = {
            let mut statement=tx.prepare("SELECT record FROM teams WHERE status IN ('running','waiting_input','verifying','blocked')")?;
            let values = statement
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            values
        };
        let mut count = 0;
        for value in encoded {
            let mut record = decode_record(&value)?;
            if record.status == TeamStatus::Blocked
                && !record.nodes.iter().any(|n| n.status.is_active())
            {
                continue;
            }
            let old = record.revision;
            record.status = TeamStatus::Interrupted;
            record.error=Some("Task service restarted; execution was interrupted and was not automatically retried".into());
            stop_active_attempts(
                &mut record,
                NodeStatus::Interrupted,
                Some("Task service restarted"),
            );
            // Close leftover reservation holds in the same transaction: the
            // interrupted attempts above are terminal, so they settle
            // conservatively at their reserved amounts; nothing leaks into a
            // future run of a fresh team on the same budget evidence.
            let reconciled: Vec<Value> = record
                .reservations
                .iter_mut()
                .filter(|reservation| reservation.status == ReservationStatus::Reserved)
                .map(|reservation| {
                    reservation.status = ReservationStatus::Settled;
                    reservation.settled_at = Some(now());
                    json!({
                        "reservation_id": reservation.id,
                        "node_id": reservation.node_id,
                        "attempt": reservation.attempt,
                        "outcome": "settled_at_reserved",
                    })
                })
                .collect();
            record.revision += 1;
            record.updated_at = now();
            tx.execute("UPDATE teams SET status=?1,revision=?2,updated_at=?3,record=?4 WHERE id=?5 AND revision=?6",params![record.status.as_str(),i64::try_from(record.revision)?,record.updated_at,encode_record(&record)?,record.id,i64::try_from(old)?])?;
            insert_event(
                &tx,
                &record.id,
                "recovered_interrupted",
                json!({"reconciled_reservations": reconciled}),
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }
}

pub fn charged_microusd(record: &TeamRecord) -> u64 {
    record
        .reservations
        .iter()
        .map(|r| match r.status {
            ReservationStatus::Reserved => r.reserved_microusd,
            ReservationStatus::Settled => r.actual_microusd.unwrap_or(r.reserved_microusd),
            ReservationStatus::Released => 0,
        })
        .fold(0, u64::saturating_add)
}
pub fn scopes_overlap(left: &[String], right: &[String]) -> bool {
    left.iter().any(|a| {
        right.iter().any(|b| {
            let a = a.replace('\\', "/").to_lowercase();
            let b = b.replace('\\', "/").to_lowercase();
            a == b || a.starts_with(&(b.clone() + "/")) || b.starts_with(&(a + "/"))
        })
    })
}
fn normalize_nodes(nodes: &mut [NodeSpec]) -> Result<()> {
    for node in nodes {
        for path in &mut node.write_paths {
            *path = normalize_write_path(path)?;
        }
    }
    Ok(())
}
fn same_path(a: &str, b: &str) -> bool {
    let normalize = |p: &str| {
        p.replace('\\', "/")
            .trim_start_matches("//?/")
            .trim_end_matches('/')
            .to_lowercase()
    };
    normalize(a) == normalize(b)
}
fn validate_error(error: Option<&str>) -> Result<()> {
    if let Some(error) = error {
        bounded(error, 16_384, "error detail", true)?;
    }
    Ok(())
}
fn latest_attempt_mut<'a>(
    record: &'a mut TeamRecord,
    node_id: &str,
    number: u32,
) -> Result<&'a mut NodeAttempt> {
    let node = record
        .nodes
        .iter_mut()
        .find(|n| n.spec.id == node_id)
        .context("node not found")?;
    let attempt = node.attempts.last_mut().context("node has no attempt")?;
    ensure!(attempt.number == number, "attempt has been superseded");
    Ok(attempt)
}
fn stop_active_attempts(record: &mut TeamRecord, status: NodeStatus, error: Option<&str>) {
    for node in &mut record.nodes {
        if node.status.is_active() {
            node.status = status;
            node.error = error.map(str::to_owned);
            if let Some(attempt) = node.attempts.last_mut() {
                attempt.status = status;
                attempt.error = error.map(str::to_owned);
                attempt.finished_at = Some(now());
            }
        }
    }
}
fn validate_check_results(checks: &[CheckSpec], results: &[CheckResult]) -> Result<()> {
    ensure!(
        results.len() == checks.len(),
        "verification must contain every declared acceptance check"
    );
    let mut indices = HashSet::new();
    for result in results {
        ensure!(
            result.check_index < checks.len() && indices.insert(result.check_index),
            "invalid or duplicate check result index"
        );
        bounded(&result.output, 32_768, "check output", true)?;
        ensure!(
            !result.success || (result.exit_code == Some(0) && !result.timed_out),
            "successful check must have exit code 0 and not time out"
        );
    }
    Ok(())
}
fn validate_passed_verification(record: &TeamRecord) -> Result<()> {
    ensure!(
        !record.nodes.is_empty()
            && record
                .nodes
                .iter()
                .all(|n| n.status == NodeStatus::Succeeded),
        "all nodes must succeed"
    );
    validate_check_results(&record.request.checks, &record.verification)?;
    ensure!(
        record
            .verification
            .iter()
            .all(|c| c.success && c.exit_code == Some(0) && !c.timed_out),
        "aggregate acceptance checks have not passed"
    );
    Ok(())
}
fn encode_record(record: &TeamRecord) -> Result<String> {
    let encoded = serde_json::to_string(record)?;
    ensure!(
        encoded.len() <= MAX_RECORD_BYTES,
        "team record exceeds storage limit"
    );
    Ok(encoded)
}
fn decode_record(encoded: &str) -> Result<TeamRecord> {
    ensure!(
        encoded.len() <= MAX_RECORD_BYTES,
        "stored team record exceeds size limit"
    );
    serde_json::from_str(encoded).context("corrupted team record")
}
fn read_record(connection: &Connection, id: &str) -> Result<Option<TeamRecord>> {
    let row = connection
        .query_row(
            "SELECT record,status,revision FROM teams WHERE id=?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?;
    row.map(|(value, status, revision)| {
        let record = decode_record(&value)?;
        ensure!(
            record.id == id
                && record.status.as_str() == status
                && i64::try_from(record.revision)? == revision,
            "inconsistent team record"
        );
        Ok(record)
    })
    .transpose()
}
fn insert_event(tx: &Transaction<'_>, id: &str, kind: &str, data: Value) -> Result<TeamEvent> {
    identifier(kind, "event kind", 80)?;
    let encoded = serde_json::to_string(&data)?;
    ensure!(encoded.len() <= MAX_EVENT_BYTES, "team event too large");
    let created_at = now();
    tx.execute(
        "INSERT INTO team_events(team_id,kind,data,created_at) VALUES(?1,?2,?3,?4)",
        params![id, kind, encoded, created_at],
    )?;
    Ok(TeamEvent {
        seq: tx.last_insert_rowid(),
        team_id: id.into(),
        kind: kind.into(),
        data,
        created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    fn binding() -> ExecutorBinding {
        ExecutorBinding {
            app_id: "codex".into(),
            model: "test-model".into(),
            reasoning_effort: Some("medium".into()),
        }
    }
    fn node(id: &str, dependencies: &[&str], path: &str) -> NodeSpec {
        NodeSpec {
            id: id.into(),
            objective: format!("Implement {id}"),
            dependencies: dependencies.iter().map(|v| (*v).into()).collect(),
            write_paths: vec![path.into()],
            acceptance: vec!["Acceptance command passes".into()],
            executor: None,
        }
    }
    fn request() -> TeamCreate {
        TeamCreate {
            title: "Test team".into(),
            prompt: "Implement and verify".into(),
            cwd: std::env::temp_dir()
                .join("original-team-checkout")
                .to_string_lossy()
                .into_owned(),
            strategy: TeamStrategy::Fixed,
            planner: binding(),
            candidates: vec![],
            nodes: vec![node("a", &[], "src/a"), node("b", &["a"], "src/b")],
            checks: vec![CheckSpec {
                program: "cargo".into(),
                args: vec!["test".into()],
                timeout_secs: 120,
            }],
            max_parallel: 2,
            max_duration_secs: 3600,
            max_attempts: 2,
            budget_usd: None,
            routing_policy: None,
        }
    }
    fn routing_policy() -> crate::routing::RoutingPolicy {
        crate::routing::RoutingPolicy {
            required_categories: vec!["Coding".into()],
            category_weights: std::collections::BTreeMap::new(),
            minimum_quality: Some(80.0),
            billing_channel: "api".into(),
            estimated_input_tokens: Some(100_000),
            estimated_output_tokens: Some(10_000),
            budget_usd: Some(0.2),
        }
    }
    fn running(store: &TeamStore, request: TeamCreate) -> TeamRecord {
        let team = store.create(request).unwrap();
        store
            .set_workspace(
                &team.id,
                &"a".repeat(40),
                &std::env::temp_dir().join(&team.id).to_string_lossy(),
            )
            .unwrap();
        store
            .transition(&team.id, &[TeamStatus::Planned], TeamStatus::Running, None)
            .unwrap()
    }
    fn finish(store: &TeamStore, id: &str, node: &str, number: u32) {
        store
            .attach_child(
                id,
                node,
                number,
                &format!("child-{id}-{node}-{number}"),
                &std::env::temp_dir()
                    .join(format!("worker-{id}-{node}-{number}"))
                    .to_string_lossy(),
            )
            .unwrap();
        store
            .set_attempt_status(id, node, number, NodeStatus::Verifying, None, None)
            .unwrap();
        store
            .set_attempt_status(
                id,
                node,
                number,
                NodeStatus::Succeeded,
                None,
                Some(json!({"patch_sha256":"test","checked":true})),
            )
            .unwrap();
    }
    #[test]
    fn rejects_cycles_missing_edges_duplicate_ids_and_unsafe_scopes() {
        let mut nodes = vec![node("a", &["b"], "src/a"), node("b", &["a"], "src/b")];
        assert!(validate_plan(&nodes).is_err());
        nodes[0].dependencies = vec!["missing".into()];
        assert!(validate_plan(&nodes).is_err());
        nodes[0].dependencies.clear();
        nodes[1].id = "a".into();
        assert!(validate_plan(&nodes).is_err());
        for path in [
            "../src",
            "/src",
            "C:\\src",
            "src/../secret",
            "src/.git/config",
            ".GIT/config",
            "src//a",
            "src/CON.txt",
            "src/name.",
            "src/name ",
            "src/*",
        ] {
            assert!(normalize_write_path(path).is_err(), "accepted {path}");
        }
        assert_eq!(normalize_write_path("src\\module").unwrap(), "src/module");
        assert!(scopes_overlap(&["src".into()], &["src/a".into()]));
        assert!(!scopes_overlap(&["src/a".into()], &["src/ab".into()]));
    }
    #[test]
    fn latest_event_replays_only_the_newest_event_of_requested_kind() {
        let store = TeamStore::open_in_memory().unwrap();
        let team = store.create(request()).unwrap();
        store
            .append_event(&team.id, "routing_preview", json!({"epoch":"old"}))
            .unwrap();
        let newest = store
            .append_event(&team.id, "routing_preview", json!({"epoch":"new"}))
            .unwrap();
        store
            .append_event(&team.id, "other", json!({"ignored":true}))
            .unwrap();
        let replay = store
            .latest_event(&team.id, "routing_preview")
            .unwrap()
            .unwrap();
        assert_eq!(replay.seq, newest.seq);
        assert_eq!(replay.data["epoch"], "new");
        assert!(store.latest_event(&team.id, "missing").unwrap().is_none());
    }
    #[test]
    fn saved_routing_policy_survives_reopen_and_is_automatic_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("teams.db");
        let policy = routing_policy();
        let id;
        {
            let store = TeamStore::open(&path).unwrap();
            let mut request = request();
            request.strategy = TeamStrategy::Automatic;
            request.candidates = vec![binding()];
            request.routing_policy = Some(policy.clone());
            id = store.create(request).unwrap().id;
        }
        {
            let store = TeamStore::open(&path).unwrap();
            assert_eq!(
                store.get(&id).unwrap().unwrap().request.routing_policy,
                Some(policy.clone())
            );
        }
        let mut fixed = request();
        fixed.routing_policy = Some(policy);
        assert!(TeamStore::open_in_memory().unwrap().create(fixed).is_err());
    }
    #[test]
    fn requires_checks_and_bounded_finite_budget() {
        let mut r = request();
        r.checks.clear();
        assert!(validate_create(&r).is_err());
        for budget in [f64::NAN, f64::INFINITY, -1.0, 0.0, MAX_BUDGET_USD + 1.0] {
            let mut r = request();
            r.budget_usd = Some(budget);
            assert!(validate_create(&r).is_err());
        }
        let mut r = request();
        r.max_attempts = 6;
        assert!(validate_create(&r).is_err());
        r = request();
        r.checks[0].program = "powershell.exe".into();
        assert!(validate_create(&r).is_err());
    }
    #[test]
    fn assigned_nodes_pin_distinct_models_and_automatic_requires_candidates() {
        let store = TeamStore::open_in_memory().unwrap();
        let mut other = binding();
        other.app_id = "kimi-cli".into();
        other.model = "kimi-test".into();
        let mut r = request();
        r.strategy = TeamStrategy::Assigned;
        r.candidates = vec![binding(), other.clone()];
        assert!(store.create(r.clone()).is_err());
        r.nodes[0].executor = Some(binding());
        r.nodes[1].executor = Some(other.clone());
        let team = running(&store, r);
        assert!(store
            .begin_attempt(&team.id, "a", other.clone(), None)
            .is_err());
        let a = store.begin_attempt(&team.id, "a", binding(), None).unwrap();
        finish(&store, &team.id, "a", a.number);
        assert!(store.begin_attempt(&team.id, "b", other, None).is_ok());
        let mut r = request();
        r.strategy = TeamStrategy::Automatic;
        assert!(store.create(r).is_err());
    }
    #[test]
    fn fixed_mode_rejects_model_switch() {
        let store = TeamStore::open_in_memory().unwrap();
        let team = running(&store, request());
        let mut other = binding();
        other.model = "other".into();
        assert!(store.begin_attempt(&team.id, "a", other, None).is_err());
    }
    #[test]
    fn only_one_cross_connection_start_wins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("teams.db");
        let store = TeamStore::open(&path).unwrap();
        let team = store.create(request()).unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let mut handles = vec![];
        for _ in 0..2 {
            let database = TeamStore::open(&path).unwrap();
            let id = team.id.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                database
                    .transition(&id, &[TeamStatus::Planned], TeamStatus::Running, None)
                    .is_ok()
            }));
        }
        barrier.wait();
        let won = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(won, 1);
    }
    #[test]
    fn dependencies_scope_reservations_attempt_limit_and_stale_updates() {
        let store = TeamStore::open_in_memory().unwrap();
        let mut r = request();
        r.nodes.push(node("c", &[], "src/a/file"));
        let team = running(&store, r);
        assert!(store.begin_attempt(&team.id, "b", binding(), None).is_err());
        let a = store.begin_attempt(&team.id, "a", binding(), None).unwrap();
        assert!(store.begin_attempt(&team.id, "a", binding(), None).is_err());
        assert!(store.begin_attempt(&team.id, "c", binding(), None).is_err());
        store
            .set_attempt_status(
                &team.id,
                "a",
                a.number,
                NodeStatus::Failed,
                Some("first failed"),
                None,
            )
            .unwrap();
        let retry = store.begin_attempt(&team.id, "a", binding(), None).unwrap();
        assert!(store
            .set_attempt_status(&team.id, "a", a.number, NodeStatus::Verifying, None, None)
            .is_err());
        store
            .set_attempt_status(&team.id, "a", retry.number, NodeStatus::Failed, None, None)
            .unwrap();
        assert!(store.begin_attempt(&team.id, "a", binding(), None).is_err());
    }
    #[test]
    fn plan_compare_and_swap_is_atomic_and_stops_at_first_attempt() {
        let store = TeamStore::open_in_memory().unwrap();
        let team = store.create(request()).unwrap();
        let changed = store
            .set_plan(&team.id, team.revision, vec![node("new", &[], "src/new")])
            .unwrap();
        assert!(store
            .set_plan(&team.id, team.revision, request().nodes)
            .is_err());
        assert_eq!(store.get(&team.id).unwrap().unwrap(), changed);
        store
            .set_workspace(
                &team.id,
                &"a".repeat(40),
                &std::env::temp_dir().join(&team.id).to_string_lossy(),
            )
            .unwrap();
        store
            .transition(&team.id, &[TeamStatus::Planned], TeamStatus::Running, None)
            .unwrap();
        store
            .begin_attempt(&team.id, "new", binding(), None)
            .unwrap();
        let latest = store.get(&team.id).unwrap().unwrap();
        assert!(store
            .set_plan(&team.id, latest.revision, request().nodes)
            .is_err());
    }
    #[test]
    fn cost_cannot_be_overcommitted_or_freed_while_active() {
        let store = TeamStore::open_in_memory().unwrap();
        let mut r = request();
        r.budget_usd = Some(1.0);
        r.nodes[1].dependencies.clear();
        let team = running(&store, r);
        assert!(store.begin_attempt(&team.id, "a", binding(), None).is_err());
        let a = store
            .begin_attempt(&team.id, "a", binding(), Some(0.7))
            .unwrap();
        let reservation = a.reservation_id.unwrap();
        assert!(store
            .begin_attempt(&team.id, "b", binding(), Some(0.4))
            .is_err());
        assert!(store
            .settle_reservation(&team.id, &reservation, Some(0.1))
            .is_err());
        assert!(store.release_reservation(&team.id, &reservation).is_err());
        store
            .set_attempt_status(&team.id, "a", 1, NodeStatus::Failed, None, None)
            .unwrap();
        let settled = store
            .settle_reservation(&team.id, &reservation, None)
            .unwrap();
        assert_eq!(charged_microusd(&settled), 700_000);
        assert_eq!(settled.reservations[0].actual_microusd, None);
        assert!(store.release_reservation(&team.id, &reservation).is_err());
        assert!(store
            .begin_attempt(&team.id, "b", binding(), Some(0.3))
            .is_ok());
    }
    #[test]
    fn reconciliation_closes_stale_holds_without_leaking_or_double_charging() {
        let store = TeamStore::open_in_memory().unwrap();
        let mut r = request();
        r.budget_usd = Some(1.0);
        r.nodes[1].dependencies.clear();
        let team = running(&store, r);
        let a = store
            .begin_attempt(&team.id, "a", binding(), Some(0.5))
            .unwrap();
        let b = store
            .begin_attempt(&team.id, "b", binding(), Some(0.3))
            .unwrap();
        // Node a's attempt reached a terminal state; node b never launched.
        store
            .set_attempt_status(&team.id, "a", 1, NodeStatus::Failed, None, None)
            .unwrap();
        let reconciled = store
            .reconcile_reservations(&team.id, "run failed: test")
            .unwrap();
        let find = |id: &str| reconciled.reservations.iter().find(|r| r.id == id).unwrap();
        assert_eq!(
            find(a.reservation_id.as_deref().unwrap()).status,
            ReservationStatus::Settled
        );
        assert_eq!(
            find(b.reservation_id.as_deref().unwrap()).status,
            ReservationStatus::Released
        );
        // The ended attempt keeps its reserved amount charged; the unlaunched
        // one is freed, so the team can retry within budget once the node is
        // marked interrupted the way a service restart would.
        assert_eq!(charged_microusd(&reconciled), 500_000);
        // Reconciliation is idempotent while no new holds exist.
        let again = store
            .reconcile_reservations(&team.id, "run finalized")
            .unwrap();
        assert_eq!(
            again
                .reservations
                .iter()
                .filter(|r| r.status == ReservationStatus::Reserved)
                .count(),
            0
        );
        store
            .set_attempt_status(&team.id, "b", 1, NodeStatus::Interrupted, None, None)
            .unwrap();
        assert!(store
            .begin_attempt(&team.id, "b", binding(), Some(0.4))
            .is_ok());
    }
    #[test]
    fn overspent_cost_is_recorded_and_blocks_new_work() {
        let store = TeamStore::open_in_memory().unwrap();
        let mut r = request();
        r.budget_usd = Some(1.0);
        let team = running(&store, r);
        let a = store
            .begin_attempt(&team.id, "a", binding(), Some(0.8))
            .unwrap();
        store
            .set_attempt_status(&team.id, "a", 1, NodeStatus::Failed, None, None)
            .unwrap();
        let settled = store
            .settle_reservation(&team.id, a.reservation_id.as_deref().unwrap(), Some(1.2))
            .unwrap();
        assert_eq!(settled.status, TeamStatus::Blocked);
        assert_eq!(charged_microusd(&settled), 1_200_000);
    }
    #[test]
    fn success_requires_all_nodes_and_independent_checks() {
        let store = TeamStore::open_in_memory().unwrap();
        let team = running(&store, request());
        assert!(store
            .transition(
                &team.id,
                &[TeamStatus::Running],
                TeamStatus::Verifying,
                None
            )
            .is_err());
        let a = store.begin_attempt(&team.id, "a", binding(), None).unwrap();
        assert!(store
            .set_attempt_status(
                &team.id,
                "a",
                a.number,
                NodeStatus::Succeeded,
                None,
                Some(json!({"claimed":true}))
            )
            .is_err());
        finish(&store, &team.id, "a", a.number);
        let b = store.begin_attempt(&team.id, "b", binding(), None).unwrap();
        finish(&store, &team.id, "b", b.number);
        store
            .transition(
                &team.id,
                &[TeamStatus::Running],
                TeamStatus::Verifying,
                None,
            )
            .unwrap();
        assert!(store
            .transition(
                &team.id,
                &[TeamStatus::Verifying],
                TeamStatus::Succeeded,
                None
            )
            .is_err());
        let check = CheckResult {
            check_index: 0,
            success: true,
            exit_code: Some(0),
            timed_out: false,
            output: "tests passed".into(),
            duration_ms: 100,
        };
        let mut forged = check.clone();
        forged.exit_code = Some(1);
        assert!(store.record_verification(&team.id, vec![forged]).is_err());
        store.record_verification(&team.id, vec![check]).unwrap();
        store
            .transition(
                &team.id,
                &[TeamStatus::Verifying],
                TeamStatus::Succeeded,
                None,
            )
            .unwrap();
        assert!(store
            .transition(
                &team.id,
                &[TeamStatus::Succeeded],
                TeamStatus::Running,
                None
            )
            .is_err());
    }
    #[test]
    fn opening_never_recovers_and_explicit_recovery_reconciles_reservations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("teams.db");
        let store = TeamStore::open(&path).unwrap();
        let team = running(&store, request());
        store
            .begin_attempt(&team.id, "a", binding(), Some(0.2))
            .unwrap();
        drop(store);
        let reopened = TeamStore::open(&path).unwrap();
        assert_eq!(
            reopened.get(&team.id).unwrap().unwrap().status,
            TeamStatus::Running
        );
        assert_eq!(reopened.recover_interrupted().unwrap(), 1);
        assert_eq!(reopened.recover_interrupted().unwrap(), 0);
        let recovered = reopened.get(&team.id).unwrap().unwrap();
        assert_eq!(recovered.nodes[0].status, NodeStatus::Interrupted);
        assert_eq!(charged_microusd(&recovered), 200_000);
        // Restart reconciliation settles the hold at its reserved amount
        // instead of leaving it Reserved forever.
        assert_eq!(recovered.reservations[0].status, ReservationStatus::Settled);
        let events = reopened.events(&team.id, 0, 100).unwrap();
        let recovery = events
            .iter()
            .find(|event| event.kind == "recovered_interrupted")
            .unwrap();
        assert_eq!(
            recovery.data["reconciled_reservations"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            recovery.data["reconciled_reservations"][0]["outcome"],
            "settled_at_reserved"
        );
        assert!(reopened
            .begin_attempt(&team.id, "a", binding(), None)
            .is_err());
        let page = reopened.events(&team.id, 0, 2).unwrap();
        assert_eq!(page.len(), 2);
        let next = reopened.events(&team.id, page[1].seq, 100).unwrap();
        assert!(next.iter().all(|event| event.seq > page[1].seq));
    }
}
