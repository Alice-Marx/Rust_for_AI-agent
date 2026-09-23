//! Durable, single-node workflow records for the desktop and local service.
//!
//! This store owns state and an ordered event history, not worker processes.
//! Opening a database never starts or retries work. A service must explicitly
//! call `recover_interrupted` once it has become the sole workflow owner.
//! Execution completion is `Verifying`; only a separate verification decision
//! may move it to `Succeeded`. API handlers must not expose arbitrary status
//! mutations to clients.

use std::{
    path::Path,
    str::FromStr,
    sync::{Mutex, MutexGuard},
    time::Duration,
};

use anyhow::{bail, ensure, Context, Result};
use chrono::{SecondsFormat, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::schedule_store::{
    ensure_future_run_at, normalize_run_at, validate_schedule_id, validate_template_size,
    ScheduleMaterialization, ScheduleRecord, ScheduleStatus, ScheduleTrigger,
    ScheduleTriggerOutcome,
};

pub const MAX_PROMPT_BYTES: usize = 262_144;
pub const MAX_OUTPUT_BYTES: usize = 1_048_576;
pub const MAX_EVENT_BYTES: usize = 262_144;
pub const DEFAULT_MAX_DURATION_SECS: u64 = 1_800;
const MAX_DURATION_SECS: u64 = 86_400;
const MAX_TITLE_BYTES: usize = 512;
const MAX_PATH_BYTES: usize = 8_192;
const MAX_IDENTIFIER_BYTES: usize = 256;
const MAX_DETAIL_BYTES: usize = 16_384;
const MAX_ACCEPTANCE_ITEMS: usize = 64;
const MAX_ACCEPTANCE_ITEM_BYTES: usize = 2_048;
const MAX_ACCEPTANCE_BYTES: usize = 65_536;
const SCHEMA_VERSION: i64 = 3;
const RECORD_COLUMNS: &str = "id,title,prompt,cwd,mode,app_id,model,read_only,max_duration_secs,acceptance,status,created_at,updated_at,output,error,native_session_id,reasoning_effort";

fn default_duration() -> u64 {
    DEFAULT_MAX_DURATION_SECS
}

fn default_mode() -> String {
    "work".into()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkflowCreate {
    #[serde(default)]
    pub title: String,
    pub prompt: String,
    pub cwd: String,
    #[serde(default = "default_mode")]
    pub mode: String,
    pub app_id: String,
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default = "default_duration")]
    pub max_duration_secs: u64,
    #[serde(default)]
    pub acceptance: Vec<String>,
}

impl Default for WorkflowCreate {
    fn default() -> Self {
        Self {
            title: String::new(),
            prompt: String::new(),
            cwd: String::new(),
            mode: default_mode(),
            app_id: String::new(),
            model: String::new(),
            reasoning_effort: None,
            read_only: false,
            max_duration_secs: default_duration(),
            acceptance: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStatus {
    Draft,
    Running,
    WaitingInput,
    Verifying,
    Succeeded,
    Failed,
    Cancelled,
    Blocked,
    Interrupted,
}

impl WorkflowStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
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

    pub fn is_active(self) -> bool {
        matches!(self, Self::Running | Self::WaitingInput | Self::Verifying)
    }

    /// Successful and cancelled workflows are immutable execution outcomes.
    /// Retrying those outcomes requires a new workflow; interrupted/failed work
    /// may be restarted only by an explicit service decision.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        use WorkflowStatus::*;
        match self {
            Draft => matches!(next, Running | Blocked | Cancelled),
            Running => matches!(
                next,
                WaitingInput | Verifying | Failed | Cancelled | Blocked | Interrupted
            ),
            WaitingInput => matches!(
                next,
                Running | Verifying | Failed | Cancelled | Blocked | Interrupted
            ),
            Verifying => matches!(next, Succeeded | Failed | Cancelled | Blocked | Interrupted),
            Blocked => matches!(next, Running | Cancelled | Failed),
            Failed | Interrupted => matches!(next, Running | Cancelled),
            Succeeded | Cancelled => false,
        }
    }
}

impl std::fmt::Display for WorkflowStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for WorkflowStatus {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "draft" => Ok(Self::Draft),
            "running" => Ok(Self::Running),
            "waiting_input" => Ok(Self::WaitingInput),
            "verifying" => Ok(Self::Verifying),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "blocked" => Ok(Self::Blocked),
            "interrupted" => Ok(Self::Interrupted),
            _ => bail!("未知工作流状态：{value}"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowRecord {
    pub id: String,
    pub title: String,
    pub prompt: String,
    pub cwd: String,
    pub mode: String,
    pub app_id: String,
    pub model: String,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    pub read_only: bool,
    pub max_duration_secs: u64,
    pub acceptance: Vec<String>,
    pub status: WorkflowStatus,
    pub created_at: String,
    pub updated_at: String,
    pub output: String,
    pub error: Option<String>,
    pub native_session_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WorkflowEvent {
    /// Database-wide increasing sequence; paginate per workflow with `seq > after`.
    pub seq: i64,
    pub workflow_id: String,
    pub kind: String,
    pub data: Value,
    pub created_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectRecord {
    pub id: String,
    pub name: String,
    pub cwd: String,
    pub created_at: String,
    pub updated_at: String,
}

/// A workflow database is authoritative. It is not the rebuildable session FTS
/// index. Use `Arc<WorkflowStore>` to share it; writes are serialized locally and
/// guarded by SQLite transactions across separate connections.
pub struct WorkflowStore {
    connection: Mutex<Connection>,
}

impl WorkflowStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).context("无法创建工作流数据库目录")?;
        }
        let connection = Connection::open(path).context("无法打开工作流数据库")?;
        Self::from_connection(connection)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(mut connection: Connection) -> Result<Self> {
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL;",
        )?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 =
            transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
        ensure!(
            version <= SCHEMA_VERSION,
            "工作流数据库版本 {version} 高于本程序支持的版本 {SCHEMA_VERSION}，请更新程序"
        );
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS workflows (
                id TEXT PRIMARY KEY NOT NULL,
                title TEXT NOT NULL,
                prompt TEXT NOT NULL,
                cwd TEXT NOT NULL,
                mode TEXT NOT NULL CHECK(mode IN ('work','chat')),
                app_id TEXT NOT NULL,
                model TEXT NOT NULL,
                read_only INTEGER NOT NULL CHECK(read_only IN (0,1)),
                max_duration_secs INTEGER NOT NULL CHECK(max_duration_secs BETWEEN 1 AND 86400),
                acceptance TEXT NOT NULL,
                status TEXT NOT NULL CHECK(status IN ('draft','running','waiting_input','verifying','succeeded','failed','cancelled','blocked','interrupted')),
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                output TEXT NOT NULL DEFAULT '',
                output_truncated INTEGER NOT NULL DEFAULT 0 CHECK(output_truncated IN (0,1)),
                error TEXT,
                native_session_id TEXT
            );
            CREATE INDEX IF NOT EXISTS workflows_updated ON workflows(updated_at DESC);
            CREATE INDEX IF NOT EXISTS workflows_status ON workflows(status);
            CREATE TABLE IF NOT EXISTS workflow_events (
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                workflow_id TEXT NOT NULL REFERENCES workflows(id) ON DELETE RESTRICT,
                kind TEXT NOT NULL,
                data TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS workflow_events_cursor ON workflow_events(workflow_id,seq);
            CREATE TABLE IF NOT EXISTS workflow_projects (
                id TEXT PRIMARY KEY NOT NULL,
                name TEXT NOT NULL,
                cwd TEXT NOT NULL UNIQUE,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS workflow_projects_updated ON workflow_projects(updated_at DESC);
            CREATE TABLE IF NOT EXISTS schedules (
                id TEXT PRIMARY KEY NOT NULL,
                run_at TEXT NOT NULL,
                template TEXT NOT NULL,
                status TEXT NOT NULL CHECK(status IN ('pending','fired','failed','cancelled')),
                created_at TEXT NOT NULL,
                completed_at TEXT,
                created_workflow_id TEXT REFERENCES workflows(id) ON DELETE RESTRICT,
                error TEXT
            );
            CREATE INDEX IF NOT EXISTS schedules_due ON schedules(status,run_at);
            CREATE TABLE IF NOT EXISTS schedule_triggers (
                schedule_id TEXT NOT NULL REFERENCES schedules(id) ON DELETE RESTRICT,
                scheduled_for TEXT NOT NULL,
                outcome TEXT NOT NULL CHECK(outcome IN ('created','failed')),
                workflow_id TEXT REFERENCES workflows(id) ON DELETE RESTRICT,
                error TEXT,
                created_at TEXT NOT NULL,
                PRIMARY KEY(schedule_id,scheduled_for)
            );
            CREATE INDEX IF NOT EXISTS schedule_triggers_created ON schedule_triggers(created_at DESC);",
        )?;
        if version < 2 {
            // An absent choice remains absent for existing tasks. Do not infer
            // settings from events or change the requested model on migration.
            transaction.execute_batch("ALTER TABLE workflows ADD COLUMN reasoning_effort TEXT;")?;
        }
        transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        transaction.commit()?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| anyhow::anyhow!("工作流存储锁已损坏，请重启服务并检查数据库"))
    }

    pub fn create(&self, request: WorkflowCreate) -> Result<WorkflowRecord> {
        let request = validate_request(request)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record = insert_workflow_record(&transaction, request, None)?;
        transaction.commit()?;
        Ok(record)
    }

    /// Copy a terminal workflow into a fresh draft without reusing its native session,
    /// output, or failure state. Starting remains a separate explicit operation.
    pub fn duplicate_as_draft(&self, id: &str) -> Result<WorkflowRecord> {
        validate_id(id)?;
        let source = self.get(id)?.context("task not found")?;
        ensure!(
            source.status.is_terminal(),
            "only a terminal task can be copied as a new draft"
        );
        let request = validate_request(WorkflowCreate {
            title: source.title.clone(),
            prompt: source.prompt.clone(),
            cwd: source.cwd.clone(),
            mode: source.mode.clone(),
            app_id: source.app_id.clone(),
            model: source.model.clone(),
            reasoning_effort: source.reasoning_effort.clone(),
            read_only: source.read_only,
            max_duration_secs: source.max_duration_secs,
            acceptance: source.acceptance.clone(),
        })?;

        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = required_record(&transaction, id)?;
        ensure!(
            current.status.is_terminal(),
            "source task is no longer terminal"
        );
        ensure!(
            current.updated_at == source.updated_at,
            "source task changed while it was being copied; reload and retry"
        );
        let record = insert_workflow_record(&transaction, request, Some(&current))?;
        transaction.commit()?;
        Ok(record)
    }

    /// Persist a one-shot request in the same durable database as workflows.
    /// It is intentionally only a saved Draft template; no executor is started
    /// by this method.
    pub fn create_schedule(&self, run_at: &str, request: WorkflowCreate) -> Result<ScheduleRecord> {
        let run_at = normalize_run_at(run_at)?;
        ensure_future_run_at(&run_at)?;
        let template = validate_request(request)?;
        validate_template_size(&template)?;
        let now = timestamp();
        let record = ScheduleRecord {
            id: Uuid::new_v4().to_string(),
            run_at,
            template,
            status: ScheduleStatus::Pending,
            created_at: now.clone(),
            completed_at: None,
            created_workflow_id: None,
            error: None,
        };
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO schedules (id,run_at,template,status,created_at,completed_at,created_workflow_id,error)\
             VALUES (?1,?2,?3,?4,?5,NULL,NULL,NULL)",
            params![
                record.id,
                record.run_at,
                serde_json::to_string(&record.template)?,
                record.status.as_str(),
                record.created_at,
            ],
        )?;
        transaction.commit()?;
        Ok(record)
    }

    pub fn list_schedules(&self) -> Result<Vec<ScheduleRecord>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT id,run_at,template,status,created_at,completed_at,created_workflow_id,error \
             FROM schedules ORDER BY run_at DESC,id DESC",
        )?;
        let rows = statement.query_map([], schedule_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn get_schedule(&self, id: &str) -> Result<Option<ScheduleRecord>> {
        validate_schedule_id(id)?;
        let connection = self.lock()?;
        get_schedule_record(&connection, id)
    }

    /// Cancel only a request that has not reached its scheduled instant. A
    /// completed record is immutable audit evidence.
    pub fn cancel_schedule(&self, id: &str) -> Result<ScheduleRecord> {
        validate_schedule_id(id)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous = required_schedule(&transaction, id)?;
        ensure!(
            previous.status == ScheduleStatus::Pending,
            "schedule is no longer pending"
        );
        let now = timestamp();
        let affected = transaction.execute(
            "UPDATE schedules SET status='cancelled',completed_at=?1 WHERE id=?2 AND status='pending'",
            params![now, id],
        )?;
        ensure!(
            affected == 1,
            "schedule changed while it was being cancelled"
        );
        let record = required_schedule(&transaction, id)?;
        transaction.commit()?;
        Ok(record)
    }

    /// Return only IDs that are still eligible to materialize. The actual
    /// state transition is rechecked in `materialize_due_schedule` under the
    /// same SQLite transaction that inserts the Draft.
    pub fn due_schedule_ids(&self) -> Result<Vec<String>> {
        let connection = self.lock()?;
        let now = timestamp();
        let mut statement = connection.prepare(
            "SELECT id FROM schedules WHERE status='pending' AND run_at<=?1 ORDER BY run_at,id",
        )?;
        let rows = statement.query_map([now], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Atomically create the one scheduled Draft and terminally record the
    /// trigger. `normalize` covers service-owned adapter availability and can
    /// make a chat request read-only; storage validation covers the saved
    /// request and workspace path.
    pub fn materialize_due_schedule<F>(
        &self,
        id: &str,
        normalize: F,
    ) -> Result<Option<ScheduleMaterialization>>
    where
        F: FnOnce(WorkflowCreate) -> Result<WorkflowCreate>,
    {
        validate_schedule_id(id)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(schedule) = get_schedule_record(&transaction, id)? else {
            transaction.commit()?;
            return Ok(None);
        };
        if schedule.status != ScheduleStatus::Pending || schedule.run_at > timestamp() {
            transaction.commit()?;
            return Ok(None);
        }

        let request = match validate_request(schedule.template.clone())
            .and_then(normalize)
            .and_then(validate_request)
            .and_then(|request| {
                validate_template_size(&request)?;
                Ok(request)
            }) {
            Ok(request) => request,
            Err(error) => {
                let error = schedule_error(&error);
                let now = timestamp();
                transaction.execute(
                    "INSERT INTO schedule_triggers (schedule_id,scheduled_for,outcome,workflow_id,error,created_at)\
                     VALUES (?1,?2,?3,NULL,?4,?5)",
                    params![
                        schedule.id,
                        schedule.run_at,
                        ScheduleTriggerOutcome::Failed.as_str(),
                        error,
                        now,
                    ],
                )?;
                let affected = transaction.execute(
                    "UPDATE schedules SET status='failed',completed_at=?1,error=?2 WHERE id=?3 AND status='pending'",
                    params![now, error, id],
                )?;
                ensure!(affected == 1, "schedule changed while failure was recorded");
                let record = required_schedule(&transaction, id)?;
                transaction.commit()?;
                return Ok(Some(ScheduleMaterialization::Failed {
                    schedule: record,
                    error,
                }));
            }
        };

        let workflow = insert_workflow_record(&transaction, request, None)?;
        let now = timestamp();
        // Match an explicitly created Draft: a scheduled Draft must also make
        // its workspace visible in the recent-project list. Keep that update
        // inside the same transaction as the workflow and trigger audit.
        touch_project(&transaction, &workflow.cwd, None, &now)?;
        insert_event(
            &transaction,
            &workflow.id,
            "scheduled_from",
            json!({
                "schedule_id": schedule.id,
                "scheduled_for": schedule.run_at,
                "started_automatically": false,
                "model_dispatched": false,
            }),
            &now,
        )?;
        transaction.execute(
            "INSERT INTO schedule_triggers (schedule_id,scheduled_for,outcome,workflow_id,error,created_at)\
             VALUES (?1,?2,?3,?4,NULL,?5)",
            params![
                schedule.id,
                schedule.run_at,
                ScheduleTriggerOutcome::Created.as_str(),
                workflow.id,
                now,
            ],
        )?;
        let affected = transaction.execute(
            "UPDATE schedules SET status='fired',completed_at=?1,created_workflow_id=?2,error=NULL \
             WHERE id=?3 AND status='pending'",
            params![now, workflow.id, id],
        )?;
        ensure!(
            affected == 1,
            "schedule changed while draft was being created"
        );
        let record = required_schedule(&transaction, id)?;
        transaction.commit()?;
        Ok(Some(ScheduleMaterialization::Created {
            schedule: record,
            workflow_id: workflow.id,
        }))
    }

    pub fn schedule_triggers(&self, id: &str) -> Result<Vec<ScheduleTrigger>> {
        validate_schedule_id(id)?;
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT schedule_id,scheduled_for,outcome,workflow_id,error,created_at \
             FROM schedule_triggers WHERE schedule_id=?1 ORDER BY created_at,rowid",
        )?;
        let rows = statement.query_map([id], schedule_trigger_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn list(&self) -> Result<Vec<WorkflowRecord>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(&format!("SELECT {RECORD_COLUMNS} FROM workflows ORDER BY updated_at DESC,created_at DESC,rowid DESC"))?;
        let rows = statement.query_map([], record_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn get(&self, id: &str) -> Result<Option<WorkflowRecord>> {
        validate_id(id)?;
        let connection = self.lock()?;
        get_record(&connection, id)
    }

    /// State update and its event commit atomically. `expected` is mandatory CAS
    /// input, not a replacement for the transition graph. Repeated cancellation
    /// is idempotent, including when a caller still expects the previous state.
    pub fn transition(
        &self,
        id: &str,
        expected: &[WorkflowStatus],
        new_status: WorkflowStatus,
        detail: Option<&str>,
    ) -> Result<WorkflowRecord> {
        validate_id(id)?;
        if let Some(detail) = detail {
            validate_text("状态说明", detail, MAX_DETAIL_BYTES, true)?;
        }
        ensure!(!expected.is_empty(), "状态转换必须声明预期状态");
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous = required_record(&transaction, id)?;
        if previous.status == WorkflowStatus::Cancelled && new_status == WorkflowStatus::Cancelled {
            transaction.commit()?;
            return Ok(previous);
        }
        ensure!(
            expected.contains(&previous.status),
            "工作流状态冲突：当前为 {}，请求预期为 {:?}",
            previous.status,
            expected
        );
        ensure!(
            previous.status.can_transition_to(new_status),
            "不允许的状态转换：{} -> {}",
            previous.status,
            new_status
        );
        let now = timestamp();
        let error = if matches!(
            new_status,
            WorkflowStatus::Failed | WorkflowStatus::Blocked | WorkflowStatus::Interrupted
        ) {
            detail.map(str::to_owned)
        } else {
            None
        };
        let affected = transaction.execute(
            "UPDATE workflows SET status=?1,updated_at=?2,error=?3 WHERE id=?4 AND status=?5",
            params![
                new_status.as_str(),
                now,
                error,
                id,
                previous.status.as_str()
            ],
        )?;
        ensure!(affected == 1, "工作流已被其他执行器更新，请重新读取状态");
        insert_event(
            &transaction,
            id,
            "status_changed",
            json!({"from":previous.status,"to":new_status,"detail":detail}),
            &now,
        )?;
        let record = required_record(&transaction, id)?;
        transaction.commit()?;
        Ok(record)
    }

    /// Append executor evidence, not an alternative way to mutate status. The
    /// caller controls event semantics and should never expose this as an
    /// unauthenticated generic event-injection endpoint.
    pub fn append_event(&self, id: &str, kind: &str, data: Value) -> Result<WorkflowEvent> {
        validate_id(id)?;
        validate_text("事件类型", kind, 128, false)?;
        ensure!(
            kind.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')),
            "事件类型只能包含英文、数字、下划线、横线或点"
        );
        validate_event_data(&data)?;
        let now = timestamp();
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_record_exists(&transaction, id)?;
        let event = insert_event(&transaction, id, kind, data, &now)?;
        transaction.execute(
            "UPDATE workflows SET updated_at=?1 WHERE id=?2",
            params![now, id],
        )?;
        transaction.commit()?;
        Ok(event)
    }

    /// Return events strictly after the cursor. Sequences are global, so gaps
    /// between a workflow's events are expected. Zero limit requests no events.
    pub fn events(&self, id: &str, after: i64, limit: usize) -> Result<Vec<WorkflowEvent>> {
        validate_id(id)?;
        ensure!(after >= 0, "事件游标不能为负数");
        let connection = self.lock()?;
        ensure_record_exists(&connection, id)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut statement = connection.prepare("SELECT seq,workflow_id,kind,data,created_at FROM workflow_events WHERE workflow_id=?1 AND seq>?2 ORDER BY seq LIMIT ?3")?;
        let rows =
            statement.query_map(params![id, after, limit.min(1_000) as i64], event_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Append UTF-8 output up to MAX_OUTPUT_BYTES. The accepted text and a single
    /// truncation marker are recorded in the same transaction. Later overflowing
    /// chunks do not grow the event table or silently reset earlier output.
    pub fn append_output(&self, id: &str, text: &str) -> Result<WorkflowRecord> {
        validate_id(id)?;
        ensure!(
            text.len() <= MAX_OUTPUT_BYTES,
            "单次输出超过 1 MiB，请分块提交"
        );
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous = required_record(&transaction, id)?;
        if text.is_empty() {
            transaction.commit()?;
            return Ok(previous);
        }
        ensure!(
            matches!(
                previous.status,
                WorkflowStatus::Running | WorkflowStatus::WaitingInput | WorkflowStatus::Verifying
            ),
            "当前状态 {} 不接受执行输出",
            previous.status
        );
        let already_truncated: bool = transaction.query_row(
            "SELECT output_truncated FROM workflows WHERE id=?1",
            [id],
            |row| row.get(0),
        )?;
        if already_truncated {
            transaction.commit()?;
            return Ok(previous);
        }
        let remaining = MAX_OUTPUT_BYTES.saturating_sub(previous.output.len());
        let accepted = utf8_prefix(text, remaining);
        let truncated = accepted.len() < text.len();
        if accepted.is_empty() && (!truncated || already_truncated) {
            transaction.commit()?;
            return Ok(previous);
        }
        let now = timestamp();
        let mut output = previous.output;
        output.push_str(accepted);
        transaction.execute(
            "UPDATE workflows SET output=?1,output_truncated=?2,updated_at=?3 WHERE id=?4",
            params![output, already_truncated || truncated, now, id],
        )?;
        // One input chunk can be larger than the event payload limit. Split only
        // event text at UTF-8 boundaries while retaining one transaction.
        let mut rest = accepted;
        while !rest.is_empty() {
            // JSON escaping can expand a codepoint up to six bytes (e.g. NUL).
            let part = utf8_prefix(rest, (MAX_EVENT_BYTES - 1_024) / 6);
            insert_event(&transaction, id, "output", json!({"text":part}), &now)?;
            rest = &rest[part.len()..];
        }
        if truncated && !already_truncated {
            insert_event(
                &transaction,
                id,
                "output_truncated",
                json!({"max_bytes":MAX_OUTPUT_BYTES}),
                &now,
            )?;
        }
        let record = required_record(&transaction, id)?;
        transaction.commit()?;
        Ok(record)
    }

    /// Bind the first nonempty native session ID. Repeating the same ID is safe;
    /// silently replacing it would break recovery evidence and is rejected.
    pub fn set_session_id(&self, id: &str, session_id: &str) -> Result<WorkflowRecord> {
        validate_id(id)?;
        validate_text("原生会话 ID", session_id, 1_024, false)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous = required_record(&transaction, id)?;
        if let Some(existing) = previous.native_session_id.as_deref() {
            ensure!(
                existing == session_id,
                "工作流已经绑定不同的原生会话，不能覆盖恢复引用"
            );
            transaction.commit()?;
            return Ok(previous);
        }
        let now = timestamp();
        transaction.execute("UPDATE workflows SET native_session_id=?1,updated_at=?2 WHERE id=?3 AND native_session_id IS NULL", params![session_id,now,id])?;
        insert_event(
            &transaction,
            id,
            "session_bound",
            json!({"native_session_id":session_id}),
            &now,
        )?;
        let record = required_record(&transaction, id)?;
        transaction.commit()?;
        Ok(record)
    }

    /// Mark unfinished native executions interrupted during service startup. This
    /// does not assert anything about the survival or outcome of native processes.
    /// Call only after acquiring the service's single-owner startup lock.
    pub fn recover_interrupted(&self) -> Result<usize> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let active = {
            // Verifying is an already-settled native run awaiting human review;
            // retaining it lets the user finish acceptance after a restart.
            let mut statement = transaction.prepare("SELECT id,status FROM workflows WHERE status IN ('running','waiting_input') ORDER BY rowid")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let now = timestamp();
        let detail = "服务启动时发现未结束的执行记录；实际进程与外部操作结果尚未核对，未自动重试";
        for (id, previous) in &active {
            let affected = transaction.execute("UPDATE workflows SET status='interrupted',error=?1,updated_at=?2 WHERE id=?3 AND status=?4", params![detail,now,id,previous])?;
            ensure!(affected == 1, "恢复记录时发生状态冲突");
            insert_event(
                &transaction,
                id,
                "status_changed",
                json!({"from":previous,"to":"interrupted","reason":"service_restart","detail":detail}),
                &now,
            )?;
        }
        transaction.commit()?;
        Ok(active.len())
    }

    /// Register a project without executing a workflow, allowing the GUI to show
    /// recent projects immediately after the user chooses a valid directory.
    pub fn upsert_project(&self, cwd: &str, name: Option<&str>) -> Result<ProjectRecord> {
        let cwd = canonical_directory(cwd)?;
        if let Some(name) = name {
            validate_text("项目名称", name, MAX_TITLE_BYTES, false)?;
        }
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let project = touch_project(&transaction, &cwd, name, &timestamp())?;
        transaction.commit()?;
        Ok(project)
    }

    pub fn list_projects(&self) -> Result<Vec<ProjectRecord>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare("SELECT id,name,cwd,created_at,updated_at FROM workflow_projects ORDER BY updated_at DESC,rowid DESC")?;
        let rows = statement.query_map([], project_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn recent_cwds(&self, limit: usize) -> Result<Vec<String>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT cwd FROM workflow_projects ORDER BY updated_at DESC,rowid DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit.min(200) as i64], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true)
}

fn validate_text(name: &str, value: &str, max_bytes: usize, allow_empty: bool) -> Result<()> {
    ensure!(allow_empty || !value.trim().is_empty(), "{name}不能为空");
    ensure!(value.len() <= max_bytes, "{name}超过 {max_bytes} 字节上限");
    ensure!(!value.contains('\0'), "{name}不能包含空字符");
    Ok(())
}

fn validate_id(id: &str) -> Result<()> {
    validate_text("工作流 ID", id, MAX_IDENTIFIER_BYTES, false)
}

fn canonical_directory(cwd: &str) -> Result<String> {
    validate_text("工作目录", cwd, MAX_PATH_BYTES, false)?;
    let path = Path::new(cwd)
        .canonicalize()
        .context("工作目录不存在或无法访问")?;
    ensure!(path.is_dir(), "工作目录必须是目录，不能是文件");
    let path = path
        .to_str()
        .context("工作目录不是有效的 Unicode 路径")?
        .to_owned();
    validate_text("规范工作目录", &path, MAX_PATH_BYTES, false)?;
    Ok(path)
}

fn validate_request(mut request: WorkflowCreate) -> Result<WorkflowCreate> {
    validate_text("任务输入", &request.prompt, MAX_PROMPT_BYTES, false)?;
    validate_text("任务标题", &request.title, MAX_TITLE_BYTES, true)?;
    if request.title.trim().is_empty() {
        request.title = request
            .prompt
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("新任务")
            .trim()
            .chars()
            .take(80)
            .collect();
    }
    request.cwd = canonical_directory(&request.cwd)?;
    ensure!(
        matches!(request.mode.as_str(), "work" | "chat"),
        "工作流模式必须是 work 或 chat"
    );
    validate_text("应用 ID", &request.app_id, MAX_IDENTIFIER_BYTES, false)?;
    validate_text("模型 ID", &request.model, MAX_IDENTIFIER_BYTES, true)?;
    if let Some(effort) = &request.reasoning_effort {
        validate_text("推理档位", effort, MAX_IDENTIFIER_BYTES, false)?;
    }
    ensure!(
        (1..=MAX_DURATION_SECS).contains(&request.max_duration_secs),
        "最长运行时间必须在 1 到 86400 秒之间"
    );
    ensure!(
        request.acceptance.len() <= MAX_ACCEPTANCE_ITEMS,
        "验收条件最多 64 项"
    );
    for item in &request.acceptance {
        validate_text("验收条件", item, MAX_ACCEPTANCE_ITEM_BYTES, false)?;
    }
    ensure!(
        request.acceptance.iter().map(String::len).sum::<usize>() <= MAX_ACCEPTANCE_BYTES,
        "验收条件总长度超过 64 KiB"
    );
    Ok(request)
}

fn insert_workflow_record(
    connection: &Connection,
    request: WorkflowCreate,
    source: Option<&WorkflowRecord>,
) -> Result<WorkflowRecord> {
    let now = timestamp();
    let record = WorkflowRecord {
        id: Uuid::new_v4().to_string(),
        title: request.title,
        prompt: request.prompt,
        cwd: request.cwd,
        mode: request.mode,
        app_id: request.app_id,
        model: request.model,
        reasoning_effort: request.reasoning_effort,
        read_only: request.read_only,
        max_duration_secs: request.max_duration_secs,
        acceptance: request.acceptance,
        status: WorkflowStatus::Draft,
        created_at: now.clone(),
        updated_at: now.clone(),
        output: String::new(),
        error: None,
        native_session_id: None,
    };
    connection.execute(
        "INSERT INTO workflows (id,title,prompt,cwd,mode,app_id,model,read_only,max_duration_secs,acceptance,status,created_at,updated_at,output,reasoning_effort)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'draft',?11,?11,'',?12)",
        params![record.id,record.title,record.prompt,record.cwd,record.mode,record.app_id,record.model,record.read_only,record.max_duration_secs as i64,serde_json::to_string(&record.acceptance)?,now,record.reasoning_effort],
    )?;
    insert_event(
        connection,
        &record.id,
        "created",
        json!({"status":"draft","app_id":record.app_id,"mode":record.mode}),
        &now,
    )?;
    if let Some(source) = source {
        insert_event(
            connection,
            &record.id,
            "duplicated_from",
            json!({
                "source_workflow_id": source.id,
                "source_status": source.status,
                "native_session_reused": false,
                "output_copied": false,
                "started_automatically": false,
            }),
            &now,
        )?;
    }
    touch_project(connection, &record.cwd, None, &now)?;
    Ok(record)
}

fn utf8_prefix(value: &str, bytes: usize) -> &str {
    let mut end = bytes.min(value.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn get_record(connection: &Connection, id: &str) -> Result<Option<WorkflowRecord>> {
    Ok(connection
        .query_row(
            &format!("SELECT {RECORD_COLUMNS} FROM workflows WHERE id=?1"),
            [id],
            record_from_row,
        )
        .optional()?)
}

fn required_record(connection: &Connection, id: &str) -> Result<WorkflowRecord> {
    get_record(connection, id)?.with_context(|| format!("工作流不存在：{id}"))
}

fn get_schedule_record(connection: &Connection, id: &str) -> Result<Option<ScheduleRecord>> {
    Ok(connection
        .query_row(
            "SELECT id,run_at,template,status,created_at,completed_at,created_workflow_id,error \
             FROM schedules WHERE id=?1",
            [id],
            schedule_from_row,
        )
        .optional()?)
}

fn required_schedule(connection: &Connection, id: &str) -> Result<ScheduleRecord> {
    get_schedule_record(connection, id)?.with_context(|| format!("schedule not found: {id}"))
}

fn ensure_record_exists(connection: &Connection, id: &str) -> Result<()> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM workflows WHERE id=?1)",
        [id],
        |row| row.get(0),
    )?;
    ensure!(exists, "工作流不存在：{id}");
    Ok(())
}

fn json_from_column<T: serde::de::DeserializeOwned>(
    row: &Row<'_>,
    column: usize,
) -> rusqlite::Result<T> {
    let text: String = row.get(column)?;
    serde_json::from_str(&text).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn record_from_row(row: &Row<'_>) -> rusqlite::Result<WorkflowRecord> {
    let status_text: String = row.get(10)?;
    let status = WorkflowStatus::from_str(&status_text).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            10,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                error.to_string(),
            )),
        )
    })?;
    Ok(WorkflowRecord {
        id: row.get(0)?,
        title: row.get(1)?,
        prompt: row.get(2)?,
        cwd: row.get(3)?,
        mode: row.get(4)?,
        app_id: row.get(5)?,
        model: row.get(6)?,
        reasoning_effort: row.get(16)?,
        read_only: row.get(7)?,
        max_duration_secs: row.get::<_, i64>(8)? as u64,
        acceptance: json_from_column(row, 9)?,
        status,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
        output: row.get(13)?,
        error: row.get(14)?,
        native_session_id: row.get(15)?,
    })
}

fn validate_event_data(data: &Value) -> Result<String> {
    let serialized = serde_json::to_string(data)?;
    ensure!(
        serialized.len() <= MAX_EVENT_BYTES,
        "事件内容超过 256 KiB，请使用产物文件引用或分块输出"
    );
    Ok(serialized)
}

fn insert_event(
    connection: &Connection,
    id: &str,
    kind: &str,
    data: Value,
    now: &str,
) -> Result<WorkflowEvent> {
    let serialized = validate_event_data(&data)?;
    connection.execute(
        "INSERT INTO workflow_events (workflow_id,kind,data,created_at) VALUES (?1,?2,?3,?4)",
        params![id, kind, serialized, now],
    )?;
    Ok(WorkflowEvent {
        seq: connection.last_insert_rowid(),
        workflow_id: id.into(),
        kind: kind.into(),
        data,
        created_at: now.into(),
    })
}

fn event_from_row(row: &Row<'_>) -> rusqlite::Result<WorkflowEvent> {
    Ok(WorkflowEvent {
        seq: row.get(0)?,
        workflow_id: row.get(1)?,
        kind: row.get(2)?,
        data: json_from_column(row, 3)?,
        created_at: row.get(4)?,
    })
}

fn project_from_row(row: &Row<'_>) -> rusqlite::Result<ProjectRecord> {
    Ok(ProjectRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        cwd: row.get(2)?,
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
    })
}

fn schedule_from_row(row: &Row<'_>) -> rusqlite::Result<ScheduleRecord> {
    let status_text: String = row.get(3)?;
    let status = ScheduleStatus::parse(&status_text).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                error.to_string(),
            )),
        )
    })?;
    Ok(ScheduleRecord {
        id: row.get(0)?,
        run_at: row.get(1)?,
        template: json_from_column(row, 2)?,
        status,
        created_at: row.get(4)?,
        completed_at: row.get(5)?,
        created_workflow_id: row.get(6)?,
        error: row.get(7)?,
    })
}

fn schedule_trigger_from_row(row: &Row<'_>) -> rusqlite::Result<ScheduleTrigger> {
    let outcome_text: String = row.get(2)?;
    let outcome = ScheduleTriggerOutcome::parse(&outcome_text).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                error.to_string(),
            )),
        )
    })?;
    Ok(ScheduleTrigger {
        schedule_id: row.get(0)?,
        scheduled_for: row.get(1)?,
        outcome,
        workflow_id: row.get(3)?,
        error: row.get(4)?,
        created_at: row.get(5)?,
    })
}

fn schedule_error(error: &anyhow::Error) -> String {
    const MAX_SCHEDULE_ERROR_BYTES: usize = 4_096;
    let text = error.to_string();
    utf8_prefix(&text, MAX_SCHEDULE_ERROR_BYTES).to_owned()
}

fn touch_project(
    connection: &Connection,
    cwd: &str,
    name: Option<&str>,
    now: &str,
) -> Result<ProjectRecord> {
    let default_name = Path::new(cwd)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(cwd);
    let title = name.unwrap_or_else(|| utf8_prefix(default_name, MAX_TITLE_BYTES));
    connection.execute(
        "INSERT INTO workflow_projects (id,name,cwd,created_at,updated_at) VALUES (?1,?2,?3,?4,?4)
         ON CONFLICT(cwd) DO UPDATE SET updated_at=excluded.updated_at,name=CASE WHEN ?5 THEN excluded.name ELSE workflow_projects.name END",
        params![Uuid::new_v4().to_string(),title,cwd,now,name.is_some()],
    )?;
    Ok(connection.query_row(
        "SELECT id,name,cwd,created_at,updated_at FROM workflow_projects WHERE cwd=?1",
        [cwd],
        project_from_row,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(cwd: &Path) -> WorkflowCreate {
        WorkflowCreate {
            title: "完善应用".into(),
            prompt: "读取项目并实现功能，随后执行验收。".into(),
            cwd: cwd.display().to_string(),
            app_id: "kimi-cli".into(),
            model: "kimi-k2.5".into(),
            acceptance: vec!["测试通过且保留现有用户修改".into()],
            ..Default::default()
        }
    }

    fn start(store: &WorkflowStore, cwd: &Path) -> WorkflowRecord {
        let record = store.create(request(cwd)).unwrap();
        store
            .transition(
                &record.id,
                &[WorkflowStatus::Draft],
                WorkflowStatus::Running,
                None,
            )
            .unwrap()
    }

    fn future_run_at() -> String {
        (Utc::now() + chrono::Duration::minutes(5)).to_rfc3339_opts(SecondsFormat::Micros, true)
    }

    fn make_schedule_due(store: &WorkflowStore, id: &str) {
        store
            .lock()
            .unwrap()
            .execute(
                "UPDATE schedules SET run_at=?1 WHERE id=?2",
                params![timestamp(), id],
            )
            .unwrap();
    }

    #[test]
    fn version_one_database_migrates_without_changing_history_and_reopens() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("legacy.sqlite");
        {
            // This is the released v1 schema, deliberately created without the
            // current store so this test exercises a real on-disk migration.
            let connection = Connection::open(&database).unwrap();
            connection.execute_batch(
                "CREATE TABLE workflows (
                    id TEXT PRIMARY KEY NOT NULL,
                    title TEXT NOT NULL, prompt TEXT NOT NULL, cwd TEXT NOT NULL,
                    mode TEXT NOT NULL CHECK(mode IN ('work','chat')),
                    app_id TEXT NOT NULL, model TEXT NOT NULL,
                    read_only INTEGER NOT NULL CHECK(read_only IN (0,1)),
                    max_duration_secs INTEGER NOT NULL CHECK(max_duration_secs BETWEEN 1 AND 86400),
                    acceptance TEXT NOT NULL,
                    status TEXT NOT NULL CHECK(status IN ('draft','running','waiting_input','verifying','succeeded','failed','cancelled','blocked','interrupted')),
                    created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                    output TEXT NOT NULL DEFAULT '',
                    output_truncated INTEGER NOT NULL DEFAULT 0 CHECK(output_truncated IN (0,1)),
                    error TEXT, native_session_id TEXT
                );
                CREATE TABLE workflow_events (
                    seq INTEGER PRIMARY KEY AUTOINCREMENT,
                    workflow_id TEXT NOT NULL REFERENCES workflows(id) ON DELETE RESTRICT,
                    kind TEXT NOT NULL, data TEXT NOT NULL, created_at TEXT NOT NULL
                );
                CREATE TABLE workflow_projects (
                    id TEXT PRIMARY KEY NOT NULL, name TEXT NOT NULL,
                    cwd TEXT NOT NULL UNIQUE, created_at TEXT NOT NULL, updated_at TEXT NOT NULL
                );
                PRAGMA user_version = 1;",
            ).unwrap();
            connection.execute(
                "INSERT INTO workflows (id,title,prompt,cwd,mode,app_id,model,read_only,max_duration_secs,acceptance,status,created_at,updated_at,output,native_session_id)
                 VALUES ('legacy','旧任务','Keep history',?1,'work','codex','gpt-test',0,30,'[\"Review output\"]','verifying','2026-09-01','2026-09-02','Existing output','native-legacy')",
                [temp.path().canonicalize().unwrap().to_string_lossy().into_owned()],
            ).unwrap();
            connection.execute_batch(
                "INSERT INTO workflow_events (seq,workflow_id,kind,data,created_at)
                 VALUES (41,'legacy','execution_settings','{\"reasoning_effort\":\"high\"}','2026-09-02');",
            ).unwrap();
        }
        let (legacy, event, new);
        {
            let store = WorkflowStore::open(&database).unwrap();
            legacy = store.get("legacy").unwrap().unwrap();
            // Old ephemeral settings are evidence, not a new durable choice.
            assert_eq!(legacy.reasoning_effort, None);
            assert_eq!(legacy.status, WorkflowStatus::Verifying);
            assert_eq!(legacy.output, "Existing output");
            assert_eq!(legacy.native_session_id.as_deref(), Some("native-legacy"));
            assert_eq!(legacy.acceptance, vec!["Review output"]);
            event = store.events("legacy", 0, 10).unwrap();
            assert_eq!(event[0].seq, 41);
            assert_eq!(event[0].data["reasoning_effort"], "high");
            let mut input = request(temp.path());
            input.app_id = "claude".into();
            input.model = "claude-sonnet-4-6".into();
            input.reasoning_effort = Some("xhigh".into());
            new = store.create(input).unwrap();
            assert_eq!(store.list().unwrap().len(), 2);
            assert!(store.events(&new.id, 0, 10).unwrap()[0].seq > 41);
            let version: i64 = store
                .lock()
                .unwrap()
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .unwrap();
            assert_eq!(version, 3);
            assert!(store.list_schedules().unwrap().is_empty());
        }
        let reopened = WorkflowStore::open(&database).unwrap();
        assert_eq!(reopened.get("legacy").unwrap(), Some(legacy));
        assert_eq!(reopened.events("legacy", 0, 10).unwrap(), event);
        assert_eq!(reopened.get(&new.id).unwrap(), Some(new.clone()));
        assert_eq!(new.reasoning_effort.as_deref(), Some("xhigh"));
    }

    #[test]
    fn old_json_defaults_effort_and_explicit_choice_survives_storage() {
        let temp = tempfile::tempdir().unwrap();
        let input: WorkflowCreate = serde_json::from_value(json!({
            "prompt":"existing client", "cwd":temp.path(),
            "app_id":"codex", "model":"gpt-test"
        }))
        .unwrap();
        assert_eq!(input.reasoning_effort, None);
        let store = WorkflowStore::open_in_memory().unwrap();
        let record = store.create(input).unwrap();
        let mut old_record_json = serde_json::to_value(&record).unwrap();
        old_record_json
            .as_object_mut()
            .unwrap()
            .remove("reasoning_effort");
        assert_eq!(
            serde_json::from_value::<WorkflowRecord>(old_record_json).unwrap(),
            record
        );
        let mut explicit = request(temp.path());
        explicit.app_id = "codex".into();
        explicit.model = "gpt-test".into();
        explicit.reasoning_effort = Some("high".into());
        let choice = store.create(explicit).unwrap();
        assert_eq!(
            store
                .get(&choice.id)
                .unwrap()
                .unwrap()
                .reasoning_effort
                .as_deref(),
            Some("high")
        );
    }

    #[test]
    fn terminal_task_duplicates_as_a_fresh_draft_without_reusing_session_or_output() {
        let temp = tempfile::tempdir().unwrap();
        let store = WorkflowStore::open_in_memory().unwrap();
        let original = start(&store, temp.path());
        store
            .set_session_id(&original.id, "native-session-original")
            .unwrap();
        store.append_output(&original.id, "partial output").unwrap();
        store
            .transition(
                &original.id,
                &[WorkflowStatus::Running],
                WorkflowStatus::Failed,
                Some("interrupted during work"),
            )
            .unwrap();

        let copy = store.duplicate_as_draft(&original.id).unwrap();
        assert_ne!(copy.id, original.id);
        assert_eq!(copy.status, WorkflowStatus::Draft);
        assert_eq!(copy.prompt, original.prompt);
        assert_eq!(copy.cwd, original.cwd);
        assert_eq!(copy.app_id, original.app_id);
        assert_eq!(copy.model, original.model);
        assert_eq!(copy.reasoning_effort, original.reasoning_effort);
        assert!(copy.output.is_empty());
        assert!(copy.error.is_none());
        assert!(copy.native_session_id.is_none());

        let events = store.events(&copy.id, 0, 10).unwrap();
        let source = events
            .iter()
            .find(|event| event.kind == "duplicated_from")
            .unwrap();
        assert_eq!(source.data["source_workflow_id"], original.id);
        assert_eq!(source.data["source_status"], "failed");
        assert_eq!(source.data["native_session_reused"], false);
        assert_eq!(source.data["output_copied"], false);
        assert_eq!(source.data["started_automatically"], false);
    }

    #[test]
    fn duplicate_accepts_each_terminal_status_and_rejects_each_nonterminal_status() {
        let temp = tempfile::tempdir().unwrap();
        let store = WorkflowStore::open_in_memory().unwrap();
        for status in [
            WorkflowStatus::Succeeded,
            WorkflowStatus::Failed,
            WorkflowStatus::Cancelled,
            WorkflowStatus::Interrupted,
        ] {
            let original = start(&store, temp.path());
            match status {
                WorkflowStatus::Succeeded => {
                    store
                        .transition(
                            &original.id,
                            &[WorkflowStatus::Running],
                            WorkflowStatus::Verifying,
                            None,
                        )
                        .unwrap();
                    store
                        .transition(
                            &original.id,
                            &[WorkflowStatus::Verifying],
                            WorkflowStatus::Succeeded,
                            Some("accepted"),
                        )
                        .unwrap();
                }
                WorkflowStatus::Failed
                | WorkflowStatus::Cancelled
                | WorkflowStatus::Interrupted => {
                    store
                        .transition(
                            &original.id,
                            &[WorkflowStatus::Running],
                            status,
                            Some("done"),
                        )
                        .unwrap();
                }
                _ => unreachable!(),
            }
            let copy = store.duplicate_as_draft(&original.id).unwrap();
            assert_eq!(copy.status, WorkflowStatus::Draft);
            let source = store
                .events(&copy.id, 0, 10)
                .unwrap()
                .into_iter()
                .find(|event| event.kind == "duplicated_from")
                .unwrap();
            assert_eq!(source.data["source_workflow_id"], original.id);
            assert_eq!(source.data["source_status"], status.as_str());
        }

        let draft = store.create(request(temp.path())).unwrap();
        assert!(store.duplicate_as_draft(&draft.id).is_err());
        let running = start(&store, temp.path());
        assert!(store.duplicate_as_draft(&running.id).is_err());
        let waiting = start(&store, temp.path());
        store
            .transition(
                &waiting.id,
                &[WorkflowStatus::Running],
                WorkflowStatus::WaitingInput,
                None,
            )
            .unwrap();
        assert!(store.duplicate_as_draft(&waiting.id).is_err());
        let verifying = start(&store, temp.path());
        store
            .transition(
                &verifying.id,
                &[WorkflowStatus::Running],
                WorkflowStatus::Verifying,
                None,
            )
            .unwrap();
        assert!(store.duplicate_as_draft(&verifying.id).is_err());
        let blocked = store.create(request(temp.path())).unwrap();
        store
            .transition(
                &blocked.id,
                &[WorkflowStatus::Draft],
                WorkflowStatus::Blocked,
                Some("waiting for an external condition"),
            )
            .unwrap();
        assert!(store.duplicate_as_draft(&blocked.id).is_err());
    }

    #[test]
    fn due_schedule_creates_one_audited_draft_without_starting_it() {
        let temp = tempfile::tempdir().unwrap();
        let store = WorkflowStore::open_in_memory().unwrap();
        assert!(store
            .create_schedule("not-a-date", request(temp.path()))
            .is_err());
        assert!(store
            .create_schedule(&timestamp(), request(temp.path()))
            .is_err());
        let schedule = store
            .create_schedule(&future_run_at(), request(temp.path()))
            .unwrap();
        make_schedule_due(&store, &schedule.id);
        let scheduled_for = store.get_schedule(&schedule.id).unwrap().unwrap().run_at;

        let result = store
            .materialize_due_schedule(&schedule.id, |request| Ok(request))
            .unwrap()
            .unwrap();
        let workflow_id = match result {
            ScheduleMaterialization::Created {
                schedule,
                workflow_id,
            } => {
                assert_eq!(schedule.status, ScheduleStatus::Fired);
                assert_eq!(
                    schedule.created_workflow_id.as_deref(),
                    Some(workflow_id.as_str())
                );
                workflow_id
            }
            ScheduleMaterialization::Failed { error, .. } => panic!("unexpected failure: {error}"),
        };
        let workflow = store.get(&workflow_id).unwrap().unwrap();
        assert_eq!(workflow.status, WorkflowStatus::Draft);
        assert!(workflow.native_session_id.is_none());
        assert!(workflow.output.is_empty());
        let event = store
            .events(&workflow.id, 0, 10)
            .unwrap()
            .into_iter()
            .find(|event| event.kind == "scheduled_from")
            .unwrap();
        assert_eq!(event.data["schedule_id"], schedule.id.as_str());
        assert_eq!(event.data["started_automatically"], false);
        assert_eq!(event.data["model_dispatched"], false);
        let triggers = store.schedule_triggers(&schedule.id).unwrap();
        assert_eq!(triggers.len(), 1);
        assert_eq!(triggers[0].schedule_id, schedule.id);
        assert_eq!(triggers[0].scheduled_for, scheduled_for);
        assert_eq!(triggers[0].outcome, ScheduleTriggerOutcome::Created);
        assert_eq!(
            triggers[0].workflow_id.as_deref(),
            Some(workflow_id.as_str())
        );
        assert!(triggers[0].error.is_none());
        assert!(store.due_schedule_ids().unwrap().is_empty());
        assert!(store
            .materialize_due_schedule(&schedule.id, |request| Ok(request))
            .unwrap()
            .is_none());
        assert_eq!(store.list().unwrap().len(), 1);
        assert_eq!(
            store
                .list_projects()
                .unwrap()
                .into_iter()
                .map(|project| project.cwd)
                .collect::<Vec<_>>(),
            vec![workflow.cwd]
        );
    }

    #[test]
    fn schedule_with_removed_workspace_becomes_failed_once_without_a_draft() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("scheduled-workspace");
        std::fs::create_dir(&workspace).unwrap();
        let store = WorkflowStore::open_in_memory().unwrap();
        let schedule = store
            .create_schedule(&future_run_at(), request(&workspace))
            .unwrap();
        std::fs::remove_dir(&workspace).unwrap();
        make_schedule_due(&store, &schedule.id);

        let result = store
            .materialize_due_schedule(&schedule.id, |request| Ok(request))
            .unwrap()
            .unwrap();
        match result {
            ScheduleMaterialization::Failed { schedule, error } => {
                assert_eq!(schedule.status, ScheduleStatus::Failed);
                assert_eq!(schedule.error.as_deref(), Some(error.as_str()));
                assert!(error.contains("工作目录"));
            }
            ScheduleMaterialization::Created { .. } => {
                panic!("removed workspace must not create a draft")
            }
        }
        assert!(store.list().unwrap().is_empty());
        assert_eq!(store.schedule_triggers(&schedule.id).unwrap().len(), 1);
        assert_eq!(
            store.schedule_triggers(&schedule.id).unwrap()[0].outcome,
            ScheduleTriggerOutcome::Failed
        );
        assert!(store
            .materialize_due_schedule(&schedule.id, |request| Ok(request))
            .unwrap()
            .is_none());
    }

    #[test]
    fn scheduled_draft_write_failure_rolls_back_schedule_and_workflow() {
        let temp = tempfile::tempdir().unwrap();
        let store = WorkflowStore::open_in_memory().unwrap();
        let schedule = store
            .create_schedule(&future_run_at(), request(temp.path()))
            .unwrap();
        make_schedule_due(&store, &schedule.id);
        store
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_scheduled_from BEFORE INSERT ON workflow_events \
                 WHEN NEW.kind='scheduled_from' BEGIN SELECT RAISE(ABORT,'simulated write failure'); END;",
            )
            .unwrap();

        assert!(store
            .materialize_due_schedule(&schedule.id, |request| Ok(request))
            .is_err());
        assert_eq!(
            store.get_schedule(&schedule.id).unwrap().unwrap().status,
            ScheduleStatus::Pending
        );
        assert!(store.schedule_triggers(&schedule.id).unwrap().is_empty());
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn pending_schedule_can_be_cancelled_and_never_becomes_due() {
        let temp = tempfile::tempdir().unwrap();
        let store = WorkflowStore::open_in_memory().unwrap();
        let schedule = store
            .create_schedule(&future_run_at(), request(temp.path()))
            .unwrap();
        let cancelled = store.cancel_schedule(&schedule.id).unwrap();
        assert_eq!(cancelled.status, ScheduleStatus::Cancelled);
        assert!(cancelled.completed_at.is_some());
        assert!(store.cancel_schedule(&schedule.id).is_err());
        make_schedule_due(&store, &schedule.id);
        assert!(store.due_schedule_ids().unwrap().is_empty());
        assert!(store
            .materialize_due_schedule(&schedule.id, |request| Ok(request))
            .unwrap()
            .is_none());
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn create_validates_chinese_directory_and_records_project_and_event() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("中文项目");
        std::fs::create_dir(&cwd).unwrap();
        let store = WorkflowStore::open_in_memory().unwrap();
        let record = store.create(request(&cwd.join("."))).unwrap();
        assert_eq!(record.cwd, cwd.canonicalize().unwrap().to_str().unwrap());
        assert_eq!(record.status, WorkflowStatus::Draft);
        assert_eq!(record.max_duration_secs, 1800);
        assert_eq!(store.get(&record.id).unwrap(), Some(record.clone()));
        assert_eq!(store.list().unwrap(), vec![record.clone()]);
        assert_eq!(store.events(&record.id, 0, 10).unwrap()[0].kind, "created");
        assert_eq!(store.list_projects().unwrap()[0].name, "中文项目");
        let project = store
            .upsert_project(&cwd.display().to_string(), Some("我的项目"))
            .unwrap();
        store.create(request(&cwd)).unwrap();
        assert_eq!(
            store.list_projects().unwrap(),
            vec![ProjectRecord {
                updated_at: store.list_projects().unwrap()[0].updated_at.clone(),
                ..project
            }]
        );
        assert_eq!(store.recent_cwds(10).unwrap(), vec![record.cwd]);
    }

    #[test]
    fn invalid_create_has_no_partial_database_effects() {
        let temp = tempfile::tempdir().unwrap();
        let store = WorkflowStore::open_in_memory().unwrap();
        let mut cases = Vec::new();
        let mut bad = request(temp.path());
        bad.prompt = " ".into();
        cases.push(bad);
        let mut bad = request(temp.path());
        bad.prompt = "a".repeat(MAX_PROMPT_BYTES + 1);
        cases.push(bad);
        let mut bad = request(temp.path());
        bad.title = "a".repeat(MAX_TITLE_BYTES + 1);
        cases.push(bad);
        let mut bad = request(temp.path());
        bad.mode = "execute-everything".into();
        cases.push(bad);
        let mut bad = request(temp.path());
        bad.app_id.clear();
        cases.push(bad);
        let mut bad = request(temp.path());
        bad.model = "bad\0model".into();
        cases.push(bad);
        let mut bad = request(temp.path());
        bad.max_duration_secs = 0;
        cases.push(bad);
        let mut bad = request(temp.path());
        bad.max_duration_secs = MAX_DURATION_SECS + 1;
        cases.push(bad);
        let mut bad = request(temp.path());
        bad.acceptance = vec!["x".into(); MAX_ACCEPTANCE_ITEMS + 1];
        cases.push(bad);
        let mut bad = request(temp.path());
        bad.cwd = temp.path().join("missing").display().to_string();
        cases.push(bad);
        for bad in cases {
            assert!(store.create(bad).is_err());
        }
        assert!(store.list().unwrap().is_empty());
        assert!(store.list_projects().unwrap().is_empty());
        let parsed: WorkflowCreate =
            serde_json::from_value(json!({"prompt":"hello","cwd":temp.path(),"app_id":"codex"}))
                .unwrap();
        assert_eq!(parsed.max_duration_secs, 1800);
        assert_eq!(parsed.mode, "work");
        assert_eq!(store.create(parsed).unwrap().title, "hello");
    }

    #[test]
    fn success_requires_verification_and_invalid_transitions_do_not_append_events() {
        let temp = tempfile::tempdir().unwrap();
        let store = WorkflowStore::open_in_memory().unwrap();
        let record = start(&store, temp.path());
        let before = store.events(&record.id, 0, 100).unwrap();
        assert!(store
            .transition(
                &record.id,
                &[WorkflowStatus::Running],
                WorkflowStatus::Succeeded,
                None
            )
            .is_err());
        assert!(store
            .transition(
                &record.id,
                &[WorkflowStatus::Draft],
                WorkflowStatus::Failed,
                Some("stale")
            )
            .is_err());
        assert_eq!(store.events(&record.id, 0, 100).unwrap(), before);
        store
            .transition(
                &record.id,
                &[WorkflowStatus::Running],
                WorkflowStatus::Verifying,
                None,
            )
            .unwrap();
        let success = store
            .transition(
                &record.id,
                &[WorkflowStatus::Verifying],
                WorkflowStatus::Succeeded,
                Some("独立验收通过"),
            )
            .unwrap();
        assert_eq!(success.status, WorkflowStatus::Succeeded);
        assert!(store
            .transition(
                &record.id,
                &[WorkflowStatus::Succeeded],
                WorkflowStatus::Running,
                None
            )
            .is_err());
    }

    #[test]
    fn repeated_cancel_is_idempotent_and_stale_completion_cannot_win() {
        let temp = tempfile::tempdir().unwrap();
        let store = WorkflowStore::open_in_memory().unwrap();
        let record = start(&store, temp.path());
        let cancelled = store
            .transition(
                &record.id,
                &[WorkflowStatus::Running],
                WorkflowStatus::Cancelled,
                Some("用户取消"),
            )
            .unwrap();
        let events = store.events(&record.id, 0, 100).unwrap();
        let repeated = store
            .transition(
                &record.id,
                &[WorkflowStatus::Running],
                WorkflowStatus::Cancelled,
                None,
            )
            .unwrap();
        assert_eq!(repeated, cancelled);
        assert_eq!(store.events(&record.id, 0, 100).unwrap(), events);
        assert!(store
            .transition(
                &record.id,
                &[WorkflowStatus::Running],
                WorkflowStatus::Verifying,
                None
            )
            .is_err());
        assert!(store.append_output(&record.id, "late result").is_err());
    }

    #[test]
    fn events_page_with_global_cursor_without_duplicates() {
        let temp = tempfile::tempdir().unwrap();
        let store = WorkflowStore::open_in_memory().unwrap();
        let first = store.create(request(temp.path())).unwrap();
        let second = store.create(request(temp.path())).unwrap();
        for i in 0..5 {
            store
                .append_event(&first.id, "activity", json!({"i":i}))
                .unwrap();
            store
                .append_event(&second.id, "activity", json!({"i":i}))
                .unwrap();
        }
        let mut all = Vec::new();
        let mut cursor = 0;
        loop {
            let page = store.events(&first.id, cursor, 2).unwrap();
            if page.is_empty() {
                break;
            }
            assert!(page
                .iter()
                .all(|event| event.workflow_id == first.id && event.seq > cursor));
            cursor = page.last().unwrap().seq;
            all.extend(page);
        }
        assert_eq!(all.len(), 6);
        assert!(all.windows(2).all(|events| events[0].seq < events[1].seq));
        assert!(store.events(&first.id, 0, 0).unwrap().is_empty());
        assert!(store.events(&first.id, -1, 2).is_err());
        assert!(store
            .append_event("nonexistent", "activity", Value::Null)
            .is_err());
        assert!(store
            .append_event(
                &first.id,
                "activity",
                Value::String("x".repeat(MAX_EVENT_BYTES))
            )
            .is_err());
    }

    #[test]
    fn startup_recovery_preserves_output_session_and_does_not_restart_work() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("workflows.sqlite");
        let (running, waiting, verifying, draft, done);
        {
            let store = WorkflowStore::open(&database).unwrap();
            running = start(&store, temp.path()).id;
            waiting = start(&store, temp.path()).id;
            verifying = start(&store, temp.path()).id;
            draft = store.create(request(temp.path())).unwrap().id;
            done = start(&store, temp.path()).id;
            store.append_output(&running, "已完成的部分结果").unwrap();
            store
                .set_session_id(&running, "native-session-123")
                .unwrap();
            store
                .transition(
                    &waiting,
                    &[WorkflowStatus::Running],
                    WorkflowStatus::WaitingInput,
                    None,
                )
                .unwrap();
            store
                .transition(
                    &verifying,
                    &[WorkflowStatus::Running],
                    WorkflowStatus::Verifying,
                    None,
                )
                .unwrap();
            store
                .transition(
                    &done,
                    &[WorkflowStatus::Running],
                    WorkflowStatus::Verifying,
                    None,
                )
                .unwrap();
            store
                .transition(
                    &done,
                    &[WorkflowStatus::Verifying],
                    WorkflowStatus::Succeeded,
                    None,
                )
                .unwrap();
        }
        let store = WorkflowStore::open(&database).unwrap();
        assert_eq!(
            store.get(&running).unwrap().unwrap().status,
            WorkflowStatus::Running
        );
        assert_eq!(store.recover_interrupted().unwrap(), 2);
        assert_eq!(store.recover_interrupted().unwrap(), 0);
        for id in [&running, &waiting] {
            let record = store.get(id).unwrap().unwrap();
            assert_eq!(record.status, WorkflowStatus::Interrupted);
            let events = store.events(id, 0, 100).unwrap();
            assert_eq!(events.last().unwrap().data["reason"], "service_restart");
        }
        let record = store.get(&running).unwrap().unwrap();
        assert_eq!(
            store.get(&verifying).unwrap().unwrap().status,
            WorkflowStatus::Verifying
        );
        assert_eq!(record.output, "已完成的部分结果");
        assert_eq!(
            record.native_session_id.as_deref(),
            Some("native-session-123")
        );
        assert_eq!(
            store.get(&draft).unwrap().unwrap().status,
            WorkflowStatus::Draft
        );
        assert_eq!(
            store.get(&done).unwrap().unwrap().status,
            WorkflowStatus::Succeeded
        );
    }

    #[test]
    fn bounded_output_and_event_text_round_trip_at_utf8_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let store = WorkflowStore::open_in_memory().unwrap();
        let record = start(&store, temp.path());
        let prefix = "a".repeat(MAX_OUTPUT_BYTES - 2);
        store.append_output(&record.id, &prefix).unwrap();
        let result = store.append_output(&record.id, "中文").unwrap();
        assert_eq!(result.output, prefix);
        let before = store.events(&record.id, 0, 1000).unwrap();
        store.append_output(&record.id, "中文").unwrap();
        store.append_output(&record.id, "ab").unwrap();
        assert_eq!(store.events(&record.id, 0, 1000).unwrap(), before);
        assert_eq!(
            before
                .iter()
                .filter(|event| event.kind == "output_truncated")
                .count(),
            1
        );
        let rebuilt: String = before
            .iter()
            .filter(|event| event.kind == "output")
            .map(|event| event.data["text"].as_str().unwrap())
            .collect();
        assert_eq!(rebuilt, result.output);
    }

    #[test]
    fn json_escaped_output_is_split_without_partial_commit() {
        let temp = tempfile::tempdir().unwrap();
        let store = WorkflowStore::open_in_memory().unwrap();
        let record = start(&store, temp.path());
        // A small raw chunk can produce a much larger JSON payload. Event
        // splitting must account for escaping instead of losing the whole append.
        let output = "\0\n\"".repeat(40_000);
        assert_eq!(
            store.append_output(&record.id, &output).unwrap().output,
            output
        );
        let events = store.events(&record.id, 0, 100).unwrap();
        assert!(events
            .iter()
            .all(|event| serde_json::to_string(&event.data).unwrap().len() <= MAX_EVENT_BYTES));
        let restored: String = events
            .iter()
            .filter(|event| event.kind == "output")
            .map(|event| event.data["text"].as_str().unwrap())
            .collect();
        assert_eq!(restored, output);
    }

    #[test]
    fn session_identity_cannot_silently_change() {
        let temp = tempfile::tempdir().unwrap();
        let store = WorkflowStore::open_in_memory().unwrap();
        let record = start(&store, temp.path());
        let first = store.set_session_id(&record.id, "native-1").unwrap();
        assert_eq!(store.set_session_id(&record.id, "native-1").unwrap(), first);
        assert!(store.set_session_id(&record.id, "native-2").is_err());
        assert!(store.set_session_id(&record.id, "").is_err());
        assert_eq!(
            store
                .events(&record.id, 0, 100)
                .unwrap()
                .iter()
                .filter(|event| event.kind == "session_bound")
                .count(),
            1
        );
    }

    #[test]
    fn event_insert_failure_rolls_back_status_and_output() {
        let temp = tempfile::tempdir().unwrap();
        let store = WorkflowStore::open_in_memory().unwrap();
        let record = start(&store, temp.path());
        store.lock().unwrap().execute_batch("CREATE TRIGGER fail_event BEFORE INSERT ON workflow_events BEGIN SELECT RAISE(ABORT,'simulated disk write failure'); END;").unwrap();
        assert!(store
            .transition(
                &record.id,
                &[WorkflowStatus::Running],
                WorkflowStatus::Verifying,
                None
            )
            .is_err());
        assert!(store.append_output(&record.id, "cannot commit").is_err());
        assert_eq!(store.get(&record.id).unwrap().unwrap(), record);
    }

    #[test]
    fn separate_connections_cannot_both_claim_the_same_draft() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("cas.sqlite");
        let store = WorkflowStore::open(&path).unwrap();
        let id = store.create(request(temp.path())).unwrap().id;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let path = path.clone();
                let id = id.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let store = WorkflowStore::open(path).unwrap();
                    barrier.wait();
                    store
                        .transition(&id, &[WorkflowStatus::Draft], WorkflowStatus::Running, None)
                        .is_ok()
                })
            })
            .collect();
        let winners = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(winners, 1);
        assert_eq!(store.events(&id, 0, 100).unwrap().len(), 2);
    }
}
