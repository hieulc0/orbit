//! Normalized cross-agent continuation and fallback domain models.
//!
//! Orbit owns the durable execution state. Agents are replaceable execution engines.
//!
//! An Attempt owns its workspace for its entire lifetime.
//! An Attempt may sequence multiple [`AgentExecution`] records across different agents
//! operating on the same workspace.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// Machine-readable agent termination classification.
///
/// Indicates how an agent process/provider call terminated.
/// Note: External test/validation failure does NOT belong here; validation is
/// performed by Orbit after agent termination and is captured as a [`FallbackTrigger`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminationReason {
    Success,
    AgentError,
    RateLimited,
    QuotaExhausted,
    TurnLimit,
    Timeout,
    ProcessCrash,
    CredentialError,
    Cancelled,
    InfrastructureError,
    ResourceExhausted,
    Unknown,
}

/// Lifecycle status of an individual agent execution within an attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentExecutionStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Interrupted,
}

/// Reason Orbit decided to trigger a fallback or continuation with another agent.
///
/// Distinct from [`TerminationReason`]: for example, an agent may terminate with
/// `TerminationReason::Success`, but external test validation fails, resulting in
/// `FallbackTrigger::ValidationFailed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackTrigger {
    AgentFailure,
    RateLimited,
    QuotaExhausted,
    TurnLimit,
    Timeout,
    ValidationFailed,
    InfrastructureFailure,
}

/// A single execution of an agent within an attempt's workspace.
///
/// An Attempt owns the workspace; an AgentExecution only uses it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentExecution {
    pub execution_id: String,
    pub sequence: u32,

    pub agent_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    pub started_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,

    pub status: AgentExecutionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub termination_reason: Option<TerminationReason>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,

    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: serde_json::Value,
}

impl AgentExecution {
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.execution_id.is_empty(), "execution_id required");
        ensure!(self.sequence > 0, "sequence must be >= 1");
        ensure!(!self.agent_type.is_empty(), "agent_type required");
        ensure!(self.started_at >= 0, "started_at must be non-negative");
        if let Some(finished) = self.finished_at {
            ensure!(
                finished >= self.started_at,
                "finished_at must be >= started_at"
            );
        }
        Ok(())
    }
}

/// Provider-neutral workspace snapshot capturing working tree state before a handoff.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSnapshot {
    pub baseline_revision: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub head_revision: String,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deleted_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub untracked_files: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_artifact_id: Option<String>,
}

impl WorkspaceSnapshot {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.baseline_revision.is_empty(),
            "baseline_revision required"
        );
        if let Some(sha) = &self.diff_sha256 {
            ensure!(crate::agent::valid_digest(sha), "invalid diff_sha256");
        }
        Ok(())
    }
}

/// Summary of previous execution context included in a handoff.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviousExecutionSummary {
    pub execution_id: String,
    pub agent_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub termination_reason: TerminationReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Summary reference to validation evidence for a handoff.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationSummary {
    pub command: String,
    pub exit_code: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_artifact_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// Versioned handoff structure ("handoff/v1") used to continue work with another agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffRecord {
    pub schema: String,
    pub task_id: String,
    pub attempt_id: String,
    pub from_execution_id: String,
    pub trigger: FallbackTrigger,

    pub workspace: WorkspaceSnapshot,
    pub previous_execution: PreviousExecutionSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation: Option<ValidationSummary>,

    pub created_at: i64,
}

pub const HANDOFF_SCHEMA_V1: &str = "handoff/v1";

impl HandoffRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        task_id: impl Into<String>,
        attempt_id: impl Into<String>,
        from_execution_id: impl Into<String>,
        trigger: FallbackTrigger,
        workspace: WorkspaceSnapshot,
        previous_execution: PreviousExecutionSummary,
        validation: Option<ValidationSummary>,
        created_at: i64,
    ) -> Result<Self> {
        let record = Self {
            schema: HANDOFF_SCHEMA_V1.into(),
            task_id: task_id.into(),
            attempt_id: attempt_id.into(),
            from_execution_id: from_execution_id.into(),
            trigger,
            workspace,
            previous_execution,
            validation,
            created_at,
        };
        record.validate()?;
        Ok(record)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == HANDOFF_SCHEMA_V1,
            "unsupported handoff schema"
        );
        ensure!(!self.task_id.is_empty(), "task_id required");
        ensure!(!self.attempt_id.is_empty(), "attempt_id required");
        ensure!(
            !self.from_execution_id.is_empty(),
            "from_execution_id required"
        );
        self.workspace.validate()?;
        ensure!(self.created_at >= 0, "created_at must be non-negative");
        Ok(())
    }
}
