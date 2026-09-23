//! Types and input validation for one-shot scheduled drafts.
//!
//! The durable tables and the materialization transaction live in
//! `workflow::WorkflowStore`: a schedule becoming terminal and its Draft being
//! inserted must commit together. This module deliberately has no separate
//! SQLite connection.

use anyhow::{ensure, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::workflow::WorkflowCreate;

/// Scheduler poll period while the local workbench service is running.
pub const SCHEDULE_POLL_SECS: u64 = 30;
/// A saved workflow template is bounded independently of the HTTP body limit.
pub const MAX_SCHEDULE_TEMPLATE_BYTES: usize = 524_288;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleStatus {
    Pending,
    Fired,
    Failed,
    Cancelled,
}

impl ScheduleStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Fired => "fired",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "pending" => Self::Pending,
            "fired" => Self::Fired,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            other => anyhow::bail!("unknown schedule status: {other}"),
        })
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleTriggerOutcome {
    Created,
    Failed,
}

impl ScheduleTriggerOutcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Failed => "failed",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "created" => Self::Created,
            "failed" => Self::Failed,
            other => anyhow::bail!("unknown schedule trigger outcome: {other}"),
        })
    }
}

/// A persisted one-shot request. A fired or failed record is retained for
/// audit; it is never restarted automatically.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduleRecord {
    pub id: String,
    /// Normalized RFC 3339 UTC instant, always microsecond precision.
    pub run_at: String,
    pub template: WorkflowCreate,
    pub status: ScheduleStatus,
    pub created_at: String,
    pub completed_at: Option<String>,
    pub created_workflow_id: Option<String>,
    pub error: Option<String>,
}

/// Immutable audit row for the single scheduled firing attempt.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduleTrigger {
    pub schedule_id: String,
    pub scheduled_for: String,
    pub outcome: ScheduleTriggerOutcome,
    pub workflow_id: Option<String>,
    pub error: Option<String>,
    pub created_at: String,
}

/// Result of a due-schedule materialization. The `Failed` case commits a
/// terminal audit row and creates no workflow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScheduleMaterialization {
    Created {
        schedule: ScheduleRecord,
        workflow_id: String,
    },
    Failed {
        schedule: ScheduleRecord,
        error: String,
    },
}

/// Parse an explicit-offset RFC 3339 instant and normalize it for safe SQLite
/// lexical comparisons. RFC 3339 without an offset is rejected by chrono.
pub fn normalize_run_at(value: &str) -> Result<String> {
    let parsed = DateTime::parse_from_rfc3339(value)
        .map_err(|_| anyhow::anyhow!("run_at must be an RFC 3339 timestamp with offset"))?;
    ensure!(
        parsed.timestamp_subsec_nanos() % 1_000 == 0,
        "run_at sub-second precision above microseconds is not accepted"
    );
    Ok(parsed
        .with_timezone(&Utc)
        .to_rfc3339_opts(SecondsFormat::Micros, true))
}

pub(crate) fn ensure_future_run_at(run_at: &str) -> Result<()> {
    let parsed = DateTime::parse_from_rfc3339(run_at)
        .map_err(|_| anyhow::anyhow!("stored run_at is not RFC 3339"))?
        .with_timezone(&Utc);
    ensure!(parsed > Utc::now(), "run_at must be in the future");
    Ok(())
}

pub(crate) fn validate_schedule_id(id: &str) -> Result<()> {
    ensure!(Uuid::parse_str(id).is_ok(), "invalid schedule id");
    Ok(())
}

pub(crate) fn validate_template_size(template: &WorkflowCreate) -> Result<()> {
    ensure!(
        serde_json::to_vec(template)?.len() <= MAX_SCHEDULE_TEMPLATE_BYTES,
        "schedule template exceeds size limit"
    );
    Ok(())
}
