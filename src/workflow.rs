//! Durable role-agent workflows, transitions, handoffs, and candidate evidence.
//!
//! Orbit owns durable task/workflow state.
//! Agents reason and modify. Orbit controls state transitions.
//! Verification produces evidence. Policy decides whether a gate passes.
//!
//! Workflow: software_change_v1:
//!   CREATED -> PLANNING -> IMPLEMENTING -> VERIFYING
//!     (on verify fail) -> REPAIRING -> VERIFYING
//!     (on verify pass) -> REVIEWING
//!     (on changes requested) -> REPAIRING -> VERIFYING -> REVIEWING
//!     (on approve) -> REGRESSION (final verification) -> COMPLETED
//!
//! Invariants:
//!   - Role != Provider != Runtime != Model != Credential != AgentExecution
//!   - Only one mutating role owns the workspace at a time (locked via orbit_attempt_workspace_locks)
//!   - Planner and Reviewer roles are read-only (enforceable boundary)
//!   - Handoffs are durable structured artifacts (PlanHandoff, ImplementationHandoff, ReviewDecision, FailureEvidence)
//!   - Stale reviews are invalidated if workspace state changes
//!   - Completion requires: verification PASS, review APPROVE, and final regression PASS on the exact same WorkspaceState

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use std::collections::BTreeMap;
use std::path::Path;

use crate::{
    model::id,
    verification::{VerificationPolicy, VerificationStore},
};

pub const WORKFLOW_KIND_SOFTWARE_CHANGE: &str = "software_change";
pub const WORKFLOW_VERSION_V1: u32 = 1;

/// Stage of the software_change_v1 workflow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStage {
    Created,
    Planning,
    Implementing,
    Verifying,
    Reviewing,
    Repairing,
    Regression,
    Completed,
    Failed,
    Cancelled,
    Exhausted,
}

impl WorkflowStage {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Created => "CREATED",
            Self::Planning => "PLANNING",
            Self::Implementing => "IMPLEMENTING",
            Self::Verifying => "VERIFYING",
            Self::Reviewing => "REVIEWING",
            Self::Repairing => "REPAIRING",
            Self::Regression => "REGRESSION",
            Self::Completed => "COMPLETED",
            Self::Failed => "FAILED",
            Self::Cancelled => "CANCELLED",
            Self::Exhausted => "EXHAUSTED",
        }
    }

    pub fn from_str_strict(s: &str) -> Result<Self> {
        match s {
            "CREATED" => Ok(Self::Created),
            "PLANNING" => Ok(Self::Planning),
            "IMPLEMENTING" => Ok(Self::Implementing),
            "VERIFYING" => Ok(Self::Verifying),
            "REVIEWING" => Ok(Self::Reviewing),
            "REPAIRING" => Ok(Self::Repairing),
            "REGRESSION" => Ok(Self::Regression),
            "COMPLETED" => Ok(Self::Completed),
            "FAILED" => Ok(Self::Failed),
            "CANCELLED" => Ok(Self::Cancelled),
            "EXHAUSTED" => Ok(Self::Exhausted),
            other => bail!("unknown workflow stage '{other}'"),
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Exhausted
        )
    }
}

/// Durable WorkflowRun representation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRun {
    pub id: String,
    pub task_id: String,
    pub attempt_id: String,
    pub workflow_kind: String,
    pub workflow_version: u32,
    pub status: WorkflowStage,
    pub current_stage: String,
    pub iteration: u32,
    pub max_iterations: u32,
    pub current_workspace_state_id: Option<String>,
    pub verification_policy_id: Option<String>,
    pub verification_policy_version: Option<u32>,
    pub verification_policy_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regression_policy_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regression_policy_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regression_policy_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_policy_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_policy_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_policy_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_revision: Option<String>,
    pub failure_reason: Option<String>,
    pub cancellation_reason: Option<String>,
    pub started_at_ms: i64,
    pub finished_at_ms: Option<i64>,
}

/// Workspace access model enforced on roles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceAccess {
    ReadOnly,
    ReadWrite,
}

/// Capabilities required or permitted for a role.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleCapabilities {
    pub repo_read: bool,
    pub repo_write: bool,
    pub shell: bool,
    pub structured_output: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_tool_audit_correlation:
        Option<crate::acp_capabilities::ToolAuditCorrelationCapability>,
}

/// A versioned Role Definition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleDefinition {
    pub role_id: String,
    pub version: u32,
    pub name: String,
    pub description: String,
    pub instructions: String,
    pub workspace_access: WorkspaceAccess,
    pub allowed_capabilities: RoleCapabilities,
    pub runtime_preferences: Vec<String>,
}

impl RoleDefinition {
    pub fn digest(&self) -> String {
        let serialized = serde_json::to_vec(self).unwrap_or_default();
        crate::model::digest(&serialized)
    }

    pub fn planner_v1() -> Self {
        Self {
            role_id: "planner".into(),
            version: 1,
            name: "Software Change Planner".into(),
            description: "Inspects task and repository to propose an implementation plan".into(),
            instructions: "Inspect the repository and task. Produce a structured PlanHandoff. Do not modify files.".into(),
            workspace_access: WorkspaceAccess::ReadOnly,
            allowed_capabilities: RoleCapabilities {
                repo_read: true,
                repo_write: false,
                shell: true,
                structured_output: true,
                required_tool_audit_correlation: Some(
                    crate::acp_capabilities::ToolAuditCorrelationCapability::Exact,
                ),
            },
            runtime_preferences: vec!["codex-acp".into(), "antigravity-acp".into()],
        }
    }

    pub fn implementer_v1() -> Self {
        Self {
            role_id: "implementer".into(),
            version: 1,
            name: "Software Change Implementer".into(),
            description: "Modifies repository to implement planned changes".into(),
            instructions: "Implement the required changes and exploratory tests per plan. Produce ImplementationHandoff.".into(),
            workspace_access: WorkspaceAccess::ReadWrite,
            allowed_capabilities: RoleCapabilities {
                repo_read: true,
                repo_write: true,
                shell: true,
                structured_output: true,
                required_tool_audit_correlation: Some(
                    crate::acp_capabilities::ToolAuditCorrelationCapability::Exact,
                ),
            },
            runtime_preferences: vec!["codex-acp".into(), "antigravity-acp".into()],
        }
    }

    pub fn reviewer_v1() -> Self {
        Self {
            role_id: "reviewer".into(),
            version: 1,
            name: "Software Change Reviewer".into(),
            description: "Reviews diff, plan, implementation, and verification evidence".into(),
            instructions: "Review code diff and verification evidence. Produce a structured ReviewDecision (APPROVE or CHANGES_REQUESTED). Do not modify files.".into(),
            workspace_access: WorkspaceAccess::ReadOnly,
            allowed_capabilities: RoleCapabilities {
                repo_read: true,
                repo_write: false,
                shell: true,
                structured_output: true,
                required_tool_audit_correlation: Some(
                    crate::acp_capabilities::ToolAuditCorrelationCapability::Exact,
                ),
            },
            runtime_preferences: vec!["antigravity-acp".into(), "codex-acp".into()],
        }
    }
}

/// Status of a Role Execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleExecutionStatus {
    Pending,
    Resolving,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl RoleExecutionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Resolving => "RESOLVING",
            Self::Running => "RUNNING",
            Self::Succeeded => "SUCCEEDED",
            Self::Failed => "FAILED",
            Self::Cancelled => "CANCELLED",
        }
    }

    pub fn from_str_strict(s: &str) -> Result<Self> {
        match s {
            "PENDING" => Ok(Self::Pending),
            "RESOLVING" => Ok(Self::Resolving),
            "RUNNING" => Ok(Self::Running),
            "SUCCEEDED" => Ok(Self::Succeeded),
            "FAILED" => Ok(Self::Failed),
            "CANCELLED" => Ok(Self::Cancelled),
            other => bail!("unknown role execution status '{other}'"),
        }
    }
}

/// Resolved target for runtime execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedExecutionTarget {
    pub provider: String,
    pub runtime_interface: String,
    pub credential_id: Option<String>,
    pub credential_generation: Option<u32>,
    pub requested_model: Option<String>,
    pub resolved_model: Option<String>,
    pub runtime_image_digest: Option<String>,
    pub resolution_reason: String,
}

/// A durable RoleExecution entity representing an agent responsibility.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleExecution {
    pub id: String,
    pub workflow_run_id: String,
    pub role_id: String,
    pub role_version: u32,
    pub role_digest: String,
    pub stage: String,
    pub iteration: u32,
    pub status: RoleExecutionStatus,
    pub input_workspace_state_id: Option<String>,
    pub output_workspace_state_id: Option<String>,
    pub resolved_target: Option<ResolvedExecutionTarget>,
    pub agent_execution_ids: Vec<String>,
    pub handoff_input_id: Option<String>,
    pub handoff_output_id: Option<String>,
    pub started_at_ms: i64,
    pub finished_at_ms: Option<i64>,
    pub termination_reason: Option<String>,
    pub failure_message: Option<String>,
}

/// Structured handoff types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HandoffType {
    Plan,
    Implementation,
    Review,
    FailureEvidence,
}

impl HandoffType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Plan => "PLAN",
            Self::Implementation => "IMPLEMENTATION",
            Self::Review => "REVIEW",
            Self::FailureEvidence => "FAILURE_EVIDENCE",
        }
    }

    pub fn from_str_strict(s: &str) -> Result<Self> {
        match s {
            "PLAN" => Ok(Self::Plan),
            "IMPLEMENTATION" => Ok(Self::Implementation),
            "REVIEW" => Ok(Self::Review),
            "FAILURE_EVIDENCE" => Ok(Self::FailureEvidence),
            other => bail!("unknown handoff type '{other}'"),
        }
    }
}

pub const ORBIT_HANDOFF_START: &str = "<<<ORBIT_HANDOFF_START>>>";
pub const ORBIT_HANDOFF_END: &str = "<<<ORBIT_HANDOFF_END>>>";

/// Structured plan handoff payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanHandoff {
    pub summary: String,
    pub affected_areas: Vec<String>,
    pub implementation_steps: Vec<String>,
    pub expected_files: Vec<String>,
    pub risks: Vec<String>,
    pub verification_notes: Vec<String>,
    pub open_questions: Vec<String>,
}

/// Structured implementation handoff payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImplementationHandoff {
    pub summary: String,
    pub changed_files: Vec<String>,
    pub tests_added_or_modified: Vec<String>,
    pub exploratory_commands: Vec<String>,
    pub known_limitations: Vec<String>,
    pub verification_notes: Vec<String>,
}

/// Review decision status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewDecisionStatus {
    Approve,
    ChangesRequested,
    Blocked,
}

/// Individual review finding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct RawReviewFinding {
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub severity: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub explanation: Option<String>,
    #[serde(default)]
    pub requested_change: Option<String>,
}

/// Individual review finding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReviewFinding {
    pub category: String,
    pub severity: String,
    pub path: Option<String>,
    pub explanation: String,
    pub requested_change: Option<String>,
}

impl<'de> serde::Deserialize<'de> for ReviewFinding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::String(s) => Ok(ReviewFinding {
                category: "general".into(),
                severity: "medium".into(),
                path: None,
                explanation: s,
                requested_change: None,
            }),
            serde_json::Value::Object(_) => {
                let raw: RawReviewFinding =
                    serde_json::from_value(value).map_err(D::Error::custom)?;
                Ok(ReviewFinding {
                    category: raw.category.unwrap_or_else(|| "general".into()),
                    severity: raw.severity.unwrap_or_else(|| "medium".into()),
                    path: raw.path,
                    explanation: raw.explanation.unwrap_or_default(),
                    requested_change: raw.requested_change,
                })
            }
            _ => Err(D::Error::custom(
                "expected string or object for ReviewFinding",
            )),
        }
    }
}

/// Structured review decision payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewDecision {
    pub decision: ReviewDecisionStatus,
    pub summary: String,
    pub findings: Vec<ReviewFinding>,
    pub requested_changes: Vec<String>,
    pub suggested_additional_checks: Vec<String>,
}

/// Structured failure evidence payload for repair iterations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureEvidenceHandoff {
    pub failed_stage: String,
    pub verification_run_id: Option<String>,
    pub failed_steps: Vec<String>,
    pub error_summary: String,
    pub stdout_previews: BTreeMap<String, String>,
    pub stderr_previews: BTreeMap<String, String>,
}

impl PlanHandoff {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.summary.trim().is_empty(),
            "PlanHandoff summary must not be empty"
        );
        ensure!(
            !self.implementation_steps.is_empty(),
            "PlanHandoff must contain implementation steps"
        );
        Ok(())
    }
}

impl ImplementationHandoff {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.summary.trim().is_empty(),
            "ImplementationHandoff summary must not be empty"
        );
        Ok(())
    }
}

impl ReviewDecision {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.summary.trim().is_empty(),
            "ReviewDecision summary must not be empty"
        );
        Ok(())
    }
}

pub fn extract_structured_envelope<T: serde::de::DeserializeOwned>(
    raw_output: &str,
    expected_schema: &str,
) -> Result<T> {
    let json_str = if let Some(start_idx) = raw_output.find(ORBIT_HANDOFF_START) {
        let after_start = &raw_output[start_idx + ORBIT_HANDOFF_START.len()..];
        if let Some(end_idx) = after_start.find(ORBIT_HANDOFF_END) {
            let mut s = after_start[..end_idx].trim();
            if s.starts_with("```json") {
                s = s.strip_prefix("```json").unwrap_or(s).trim();
            } else if s.starts_with("```") {
                s = s.strip_prefix("```").unwrap_or(s).trim();
            }
            if s.ends_with("```") {
                s = s.strip_suffix("```").unwrap_or(s).trim();
            }
            s
        } else {
            bail!(
                "ROLE_OUTPUT_INVALID: missing end delimiter {} for schema {}",
                ORBIT_HANDOFF_END,
                expected_schema
            );
        }
    } else {
        let mut trimmed = raw_output.trim();
        if trimmed.starts_with("```json") {
            trimmed = trimmed.strip_prefix("```json").unwrap_or(trimmed).trim();
        } else if trimmed.starts_with("```") {
            trimmed = trimmed.strip_prefix("```").unwrap_or(trimmed).trim();
        }
        if trimmed.ends_with("```") {
            trimmed = trimmed.strip_suffix("```").unwrap_or(trimmed).trim();
        }
        if (trimmed.starts_with("{") && trimmed.ends_with("}"))
            || (trimmed.starts_with("[") && trimmed.ends_with("]"))
        {
            trimmed
        } else {
            bail!(
                "ROLE_OUTPUT_INVALID: expected envelope {} not found in agent output for schema {}",
                ORBIT_HANDOFF_START,
                expected_schema
            );
        }
    };

    serde_json::from_str::<T>(json_str).map_err(|e| {
        anyhow::anyhow!(
            "ROLE_OUTPUT_INVALID: failed to deserialize payload as {}: {e}",
            expected_schema
        )
    })
}

/// Durable handoff artifact record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffArtifact {
    pub id: String,
    pub workflow_run_id: String,
    pub role_execution_id: Option<String>,
    pub handoff_type: HandoffType,
    pub version: u32,
    pub workspace_state_id: Option<String>,
    pub structured_payload: serde_json::Value,
}

/// Store for managing durable workflow operations in PostgreSQL.
#[derive(Clone)]
pub struct WorkflowStore {
    pool: PgPool,
    verification_store: VerificationStore,
    step_claim: Option<StepClaim>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepClaim {
    pub workflow_run_id: String,
    pub owner_id: String,
    pub generation: i64,
}

impl WorkflowStore {
    pub fn new(pool: PgPool) -> Self {
        let verification_store = VerificationStore::new(pool.clone());
        Self {
            pool,
            verification_store,
            step_claim: None,
        }
    }

    pub fn with_step_claim(&self, claim: StepClaim) -> Self {
        let mut owned = self.clone();
        owned.step_claim = Some(claim);
        owned
    }

    /// A claim is one short PostgreSQL update. Provider I/O happens after it commits.
    pub async fn claim_workflow_step(&self, wf_id: &str) -> Result<Option<StepClaim>> {
        let owner_id = format!("step-{}", id());
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as i64;
        let generation = sqlx::query_scalar::<_, i64>(
            "UPDATE orbit_workflow_runs SET step_owner_id = $2, step_generation = step_generation + 1, step_owner_pid = $3, step_owner_started_at_ms = $4 WHERE id = $1 AND step_owner_id IS NULL AND status NOT IN ('COMPLETED', 'FAILED', 'CANCELLED', 'EXHAUSTED') RETURNING step_generation",
        )
        .bind(wf_id)
        .bind(&owner_id)
        .bind(std::process::id() as i32)
        .bind(now_ms)
        .fetch_optional(&self.pool)
        .await?;
        Ok(generation.map(|generation| StepClaim {
            workflow_run_id: wf_id.to_owned(),
            owner_id,
            generation,
        }))
    }

    /// Reclaim a dead coordinator only when no role or verification process can
    /// still own external work. A live or uncertain owner remains fenced.
    pub async fn recover_orphaned_workflow_step(&self, wf_id: &str) -> Result<bool> {
        let row = sqlx::query(
            "SELECT step_owner_id, step_owner_pid, step_generation FROM orbit_workflow_runs WHERE id = $1",
        )
        .bind(wf_id)
        .fetch_one(&self.pool)
        .await?;
        let Some(owner_id) = row.get::<Option<String>, _>("step_owner_id") else {
            return Ok(false);
        };
        let pid: Option<i32> = row.get("step_owner_pid");
        let generation: i64 = row.get("step_generation");
        if pid.is_some_and(|pid| pid > 0 && Path::new(&format!("/proc/{pid}")).exists()) {
            return Ok(false);
        }
        let active_roles: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM orbit_role_executions WHERE workflow_run_id = $1 AND status IN ('RESOLVING', 'RUNNING')",
        )
        .bind(wf_id)
        .fetch_one(&self.pool)
        .await?;
        ensure!(
            active_roles == 0,
            "WORKFLOW_RECOVERY_REQUIRES_EXTERNAL_RECONCILIATION: an unfinished role may still have effects"
        );
        let active_verifications: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM orbit_verification_runs vr JOIN orbit_workflow_runs wf ON vr.attempt_id = wf.attempt_id WHERE wf.id = $1 AND vr.status IN ('PENDING', 'RUNNING')",
        )
        .bind(wf_id)
        .fetch_one(&self.pool)
        .await?;
        ensure!(
            active_verifications == 0,
            "WORKFLOW_RECOVERY_REQUIRES_EXTERNAL_RECONCILIATION: verification may still be running"
        );
        let mut tx = self.pool.begin().await?;
        let released = sqlx::query(
            "UPDATE orbit_workflow_runs SET step_owner_id = NULL, step_owner_pid = NULL, step_owner_started_at_ms = NULL, step_generation = step_generation + 1 WHERE id = $1 AND step_owner_id = $2 AND step_generation = $3",
        )
        .bind(wf_id)
        .bind(&owner_id)
        .bind(generation)
        .execute(&mut *tx)
        .await?;
        if released.rows_affected() == 1 {
            sqlx::query(
                "UPDATE orbit_role_executions SET status = 'FAILED', termination_reason = 'STEP_OWNER_LOST_BEFORE_ROLE_START' WHERE workflow_run_id = $1 AND status = 'PENDING'",
            )
            .bind(wf_id)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "DELETE FROM orbit_attempt_workspace_locks locks USING orbit_role_executions re WHERE locks.holder_role_execution_id = re.id AND re.workflow_run_id = $1 AND re.status IN ('SUCCEEDED', 'FAILED', 'CANCELLED') AND NOT EXISTS (SELECT 1 FROM orbit_agent_executions ae WHERE ae.role_execution_id = re.id AND (ae.status IN ('PENDING', 'RUNNING') OR COALESCE(ae.metadata->>'cleanup_confirmed', 'false') <> 'true'))",
            )
            .bind(wf_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(released.rows_affected() == 1)
    }

    pub async fn release_workflow_step(&self, claim: &StepClaim) -> Result<()> {
        let result = sqlx::query(
            "UPDATE orbit_workflow_runs SET step_owner_id = NULL, step_owner_pid = NULL, step_owner_started_at_ms = NULL WHERE id = $1 AND step_owner_id = $2 AND step_generation = $3",
        )
        .bind(&claim.workflow_run_id)
        .bind(&claim.owner_id)
        .bind(claim.generation)
        .execute(&self.pool)
        .await?;
        ensure!(result.rows_affected() == 1, "WORKFLOW_STEP_OWNER_LOST");
        Ok(())
    }

    pub async fn advance_repair_iteration(
        &self,
        wf_id: &str,
        previous: u32,
        next: u32,
    ) -> Result<()> {
        let claim = self
            .step_claim
            .as_ref()
            .context("WORKFLOW_STEP_OWNER_REQUIRED")?;
        ensure!(
            claim.workflow_run_id == wf_id,
            "WORKFLOW_STEP_OWNER_MISMATCH"
        );
        let result = sqlx::query(
            "UPDATE orbit_workflow_runs SET iteration = $1 WHERE id = $2 AND iteration = $3 AND status = 'REPAIRING' AND step_owner_id = $4 AND step_generation = $5",
        )
        .bind(next as i32)
        .bind(wf_id)
        .bind(previous as i32)
        .bind(&claim.owner_id)
        .bind(claim.generation)
        .execute(&self.pool)
        .await?;
        ensure!(result.rows_affected() == 1, "WORKFLOW_STEP_OWNER_LOST");
        Ok(())
    }

    pub fn verification_store(&self) -> &VerificationStore {
        &self.verification_store
    }

    /// Create and persist a new WorkflowRun in CREATED stage.
    pub async fn create_workflow_run(
        &self,
        task_id: &str,
        attempt_id: &str,
        max_iterations: u32,
        policy: Option<&VerificationPolicy>,
    ) -> Result<WorkflowRun> {
        self.create_workflow_run_with_regression_policy(
            task_id,
            attempt_id,
            max_iterations,
            policy,
            None,
        )
        .await
    }

    pub async fn create_workflow_run_with_regression_policy(
        &self,
        task_id: &str,
        attempt_id: &str,
        max_iterations: u32,
        policy: Option<&VerificationPolicy>,
        regression_policy: Option<&crate::regression_strategy::RegressionPolicy>,
    ) -> Result<WorkflowRun> {
        self.create_workflow_run_full(
            task_id,
            attempt_id,
            max_iterations,
            policy,
            regression_policy,
            None,
            None,
            None,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_workflow_run_full(
        &self,
        task_id: &str,
        attempt_id: &str,
        max_iterations: u32,
        policy: Option<&VerificationPolicy>,
        regression_policy: Option<&crate::regression_strategy::RegressionPolicy>,
        selection_policy: Option<&crate::regression_strategy::SelectionPolicy>,
        task_prompt: Option<&str>,
        repository_path: Option<&str>,
        base_revision: Option<&str>,
    ) -> Result<WorkflowRun> {
        let wf_id = format!("wf-{}", id());
        let canonical_repository = std::fs::canonicalize(repository_path.unwrap_or("."))
            .context("canonicalize workflow repository at creation")?;
        ensure!(
            canonical_repository.is_dir(),
            "workflow repository must be a directory"
        );
        let canonical_repository = canonical_repository
            .to_str()
            .context("workflow repository path is not UTF-8")?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as i64;

        let policy_id = policy.map(|p| p.id.clone());
        let policy_ver = policy.map(|p| p.version as i32);
        let policy_dig = policy.map(|p| p.digest());

        let reg_id = regression_policy.map(|r| r.id.clone());
        let reg_ver = regression_policy.map(|r| r.version as i32);
        let reg_dig = regression_policy.map(|r| r.digest());

        let sel_id = selection_policy.map(|s| s.id.clone());
        let sel_ver = selection_policy.map(|s| s.version as i32);
        let sel_dig = selection_policy.map(|s| s.digest());

        sqlx::query(
            r#"
            INSERT INTO orbit_workflow_runs (
                id, task_id, attempt_id, workflow_kind, workflow_version,
                status, current_stage, iteration, max_iterations,
                verification_policy_id, verification_policy_version, verification_policy_digest,
                regression_policy_id, regression_policy_version, regression_policy_digest,
                selection_policy_id, selection_policy_version, selection_policy_digest,
                task_prompt, repository_path, base_revision,
                started_at_ms
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22)
            "#,
        )
        .bind(&wf_id)
        .bind(task_id)
        .bind(attempt_id)
        .bind(WORKFLOW_KIND_SOFTWARE_CHANGE)
        .bind(WORKFLOW_VERSION_V1 as i32)
        .bind(WorkflowStage::Created.as_str())
        .bind(WorkflowStage::Created.as_str())
        .bind(1)
        .bind(max_iterations as i32)
        .bind(policy_id.as_deref())
        .bind(policy_ver)
        .bind(policy_dig.as_deref())
        .bind(reg_id.as_deref())
        .bind(reg_ver)
        .bind(reg_dig.as_deref())
        .bind(sel_id.as_deref())
        .bind(sel_ver)
        .bind(sel_dig.as_deref())
        .bind(task_prompt)
        .bind(canonical_repository)
        .bind(base_revision)
        .bind(now_ms)
        .execute(&self.pool)
        .await
        .context("insert orbit_workflow_runs")?;

        self.get_workflow_run(&wf_id)
            .await?
            .context("workflow run not found after creation")
    }

    pub async fn get_workflow_run(&self, wf_id: &str) -> Result<Option<WorkflowRun>> {
        let row = sqlx::query(
            r#"
            SELECT id, task_id, attempt_id, workflow_kind, workflow_version,
                   status, current_stage, iteration, max_iterations,
                   current_workspace_state_id, verification_policy_id,
                   verification_policy_version, verification_policy_digest,
                   regression_policy_id, regression_policy_version, regression_policy_digest,
                   selection_policy_id, selection_policy_version, selection_policy_digest,
                   task_prompt, repository_path, base_revision,
                   failure_reason, cancellation_reason, started_at_ms, finished_at_ms
            FROM orbit_workflow_runs WHERE id = $1
            "#,
        )
        .bind(wf_id)
        .fetch_optional(&self.pool)
        .await
        .context("select orbit_workflow_runs")?;

        let Some(r) = row else { return Ok(None) };
        let status_str: String = r.get("status");
        let status = WorkflowStage::from_str_strict(&status_str)?;
        let pol_ver: Option<i32> = r.get("verification_policy_version");
        let reg_ver: Option<i32> = r.get("regression_policy_version");
        let sel_ver: Option<i32> = r.try_get("selection_policy_version").ok().flatten();

        Ok(Some(WorkflowRun {
            id: r.get("id"),
            task_id: r.get("task_id"),
            attempt_id: r.get("attempt_id"),
            workflow_kind: r.get("workflow_kind"),
            workflow_version: r.get::<i32, _>("workflow_version") as u32,
            status,
            current_stage: r.get("current_stage"),
            iteration: r.get::<i32, _>("iteration") as u32,
            max_iterations: r.get::<i32, _>("max_iterations") as u32,
            current_workspace_state_id: r.get("current_workspace_state_id"),
            verification_policy_id: r.get("verification_policy_id"),
            verification_policy_version: pol_ver.map(|v| v as u32),
            verification_policy_digest: r.get("verification_policy_digest"),
            regression_policy_id: r.get("regression_policy_id"),
            regression_policy_version: reg_ver.map(|v| v as u32),
            regression_policy_digest: r.get("regression_policy_digest"),
            selection_policy_id: r.try_get("selection_policy_id").ok().flatten(),
            selection_policy_version: sel_ver.map(|v| v as u32),
            selection_policy_digest: r.try_get("selection_policy_digest").ok().flatten(),
            task_prompt: r.try_get("task_prompt").ok().flatten(),
            repository_path: r.try_get("repository_path").ok().flatten(),
            base_revision: r.try_get("base_revision").ok().flatten(),
            failure_reason: r.get("failure_reason"),
            cancellation_reason: r.get("cancellation_reason"),
            started_at_ms: r.get("started_at_ms"),
            finished_at_ms: r.get("finished_at_ms"),
        }))
    }

    pub async fn list_workflow_runs(&self, attempt_id: Option<&str>) -> Result<Vec<WorkflowRun>> {
        let rows = if let Some(att) = attempt_id {
            sqlx::query(
                r#"
                SELECT id, task_id, attempt_id, workflow_kind, workflow_version,
                       status, current_stage, iteration, max_iterations,
                       current_workspace_state_id, verification_policy_id,
                       verification_policy_version, verification_policy_digest,
                       regression_policy_id, regression_policy_version, regression_policy_digest,
                       selection_policy_id, selection_policy_version, selection_policy_digest,
                       task_prompt, repository_path, base_revision,
                       failure_reason, cancellation_reason, started_at_ms, finished_at_ms
                FROM orbit_workflow_runs WHERE attempt_id = $1 ORDER BY created_at DESC
                "#,
            )
            .bind(att)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                r#"
                SELECT id, task_id, attempt_id, workflow_kind, workflow_version,
                       status, current_stage, iteration, max_iterations,
                       current_workspace_state_id, verification_policy_id,
                       verification_policy_version, verification_policy_digest,
                       regression_policy_id, regression_policy_version, regression_policy_digest,
                       selection_policy_id, selection_policy_version, selection_policy_digest,
                       task_prompt, repository_path, base_revision,
                       failure_reason, cancellation_reason, started_at_ms, finished_at_ms
                FROM orbit_workflow_runs ORDER BY created_at DESC
                "#,
            )
            .fetch_all(&self.pool)
            .await?
        };

        let mut out = Vec::new();
        for r in rows {
            let status_str: String = r.get("status");
            let status = WorkflowStage::from_str_strict(&status_str)?;
            let pol_ver: Option<i32> = r.get("verification_policy_version");
            let reg_ver: Option<i32> = r.get("regression_policy_version");
            let sel_ver: Option<i32> = r.try_get("selection_policy_version").ok().flatten();
            out.push(WorkflowRun {
                id: r.get("id"),
                task_id: r.get("task_id"),
                attempt_id: r.get("attempt_id"),
                workflow_kind: r.get("workflow_kind"),
                workflow_version: r.get::<i32, _>("workflow_version") as u32,
                status,
                current_stage: r.get("current_stage"),
                iteration: r.get::<i32, _>("iteration") as u32,
                max_iterations: r.get::<i32, _>("max_iterations") as u32,
                current_workspace_state_id: r.get("current_workspace_state_id"),
                verification_policy_id: r.get("verification_policy_id"),
                verification_policy_version: pol_ver.map(|v| v as u32),
                verification_policy_digest: r.get("verification_policy_digest"),
                regression_policy_id: r.get("regression_policy_id"),
                regression_policy_version: reg_ver.map(|v| v as u32),
                regression_policy_digest: r.get("regression_policy_digest"),
                selection_policy_id: r.try_get("selection_policy_id").ok().flatten(),
                selection_policy_version: sel_ver.map(|v| v as u32),
                selection_policy_digest: r.try_get("selection_policy_digest").ok().flatten(),
                task_prompt: r.try_get("task_prompt").ok().flatten(),
                repository_path: r.try_get("repository_path").ok().flatten(),
                base_revision: r.try_get("base_revision").ok().flatten(),
                failure_reason: r.get("failure_reason"),
                cancellation_reason: r.get("cancellation_reason"),
                started_at_ms: r.get("started_at_ms"),
                finished_at_ms: r.get("finished_at_ms"),
            });
        }
        Ok(out)
    }

    /// Advance workflow stage durably with state transition validation.
    pub async fn transition_workflow_stage(
        &self,
        wf_id: &str,
        new_stage: WorkflowStage,
        workspace_state_id: Option<&str>,
        iteration: Option<u32>,
        reason: Option<&str>,
    ) -> Result<WorkflowRun> {
        let current = self
            .get_workflow_run(wf_id)
            .await?
            .context("workflow run not found")?;
        if let Some(claim) = &self.step_claim {
            ensure!(
                claim.workflow_run_id == wf_id,
                "WORKFLOW_STEP_OWNER_MISMATCH"
            );
        }

        if current.status.is_terminal() {
            bail!(
                "cannot transition workflow run from terminal status {:?}",
                current.status
            );
        }

        // Validate state transitions
        match (&current.status, &new_stage) {
            (WorkflowStage::Created, WorkflowStage::Planning) => {}
            (WorkflowStage::Planning, WorkflowStage::Implementing) => {}
            (WorkflowStage::Implementing, WorkflowStage::Verifying) => {}
            (WorkflowStage::Verifying, WorkflowStage::Repairing) => {}
            (WorkflowStage::Verifying, WorkflowStage::Reviewing) => {}
            (WorkflowStage::Repairing, WorkflowStage::Verifying) => {}
            (WorkflowStage::Reviewing, WorkflowStage::Repairing) => {}
            (WorkflowStage::Reviewing, WorkflowStage::Regression) => {}
            (WorkflowStage::Regression, WorkflowStage::Repairing) => {}
            (WorkflowStage::Regression, WorkflowStage::Completed) => {}
            // Terminal failure / cancellation / exhaustion can occur from non-terminal states
            (_, WorkflowStage::Failed | WorkflowStage::Cancelled | WorkflowStage::Exhausted) => {}
            (from, to) => {
                bail!("invalid workflow transition from {from:?} to {to:?}");
            }
        }

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as i64;
        let finished_ms = if new_stage.is_terminal() {
            Some(now_ms)
        } else {
            None
        };

        let failure_reason =
            if matches!(new_stage, WorkflowStage::Failed | WorkflowStage::Exhausted) {
                reason
            } else {
                None
            };
        let cancellation_reason = if new_stage == WorkflowStage::Cancelled {
            reason
        } else {
            None
        };

        let result = sqlx::query(
            r#"
            UPDATE orbit_workflow_runs
            SET status = $1,
                current_stage = $2,
                current_workspace_state_id = COALESCE($3, current_workspace_state_id),
                iteration = COALESCE($4, iteration),
                finished_at_ms = COALESCE($5, finished_at_ms),
                failure_reason = COALESCE($6, failure_reason),
                cancellation_reason = COALESCE($7, cancellation_reason)
            WHERE id = $8 AND status = $9
              AND (($10::text IS NULL AND step_owner_id IS NULL)
                   OR (step_owner_id = $10 AND step_generation = $11))
            "#,
        )
        .bind(new_stage.as_str())
        .bind(new_stage.as_str())
        .bind(workspace_state_id)
        .bind(iteration.map(|i| i as i32))
        .bind(finished_ms)
        .bind(failure_reason)
        .bind(cancellation_reason)
        .bind(wf_id)
        .bind(current.status.as_str())
        .bind(
            self.step_claim
                .as_ref()
                .map(|claim| claim.owner_id.as_str()),
        )
        .bind(self.step_claim.as_ref().map(|claim| claim.generation))
        .execute(&self.pool)
        .await
        .context("update orbit_workflow_runs status")?;
        ensure!(result.rows_affected() == 1, "WORKFLOW_STAGE_FENCE_REJECTED");

        self.get_workflow_run(wf_id)
            .await?
            .context("workflow run not found after update")
    }

    /// Acquire exclusion for the persisted role and canonical repository candidate.
    pub async fn acquire_workspace_mutation_lock(
        &self,
        attempt_id: &str,
        role_execution_id: &str,
    ) -> Result<()> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as i64;

        let res = sqlx::query(
            r#"
            INSERT INTO orbit_attempt_workspace_locks
                (attempt_id, holder_role_execution_id, acquired_at_ms, workspace_identity)
            SELECT wf.attempt_id, re.id, $3, wf.repository_path
            FROM orbit_role_executions re
            JOIN orbit_workflow_runs wf ON wf.id = re.workflow_run_id
            WHERE re.id = $2 AND wf.attempt_id = $1 AND wf.repository_path IS NOT NULL
              AND wf.status NOT IN ('COMPLETED', 'FAILED', 'CANCELLED', 'EXHAUSTED')
            FOR UPDATE OF wf
            "#,
        )
        .bind(attempt_id)
        .bind(role_execution_id)
        .bind(now_ms)
        .execute(&self.pool)
        .await;

        match res {
            Ok(result) if result.rows_affected() == 1 => Ok(()),
            Ok(_) => bail!("workspace mutation lock requires a matching persisted role execution"),
            Err(e) => {
                bail!(
                    "workspace mutation lock already held for canonical workspace or attempt '{attempt_id}': {e}"
                );
            }
        }
    }

    /// Release the exclusive workspace mutation lock.
    pub async fn release_workspace_mutation_lock(
        &self,
        attempt_id: &str,
        role_execution_id: &str,
    ) -> Result<()> {
        let res = sqlx::query(
            r#"
            DELETE FROM orbit_attempt_workspace_locks
            WHERE attempt_id = $1 AND holder_role_execution_id = $2
            "#,
        )
        .bind(attempt_id)
        .bind(role_execution_id)
        .execute(&self.pool)
        .await
        .context("delete orbit_attempt_workspace_locks")?;

        ensure!(
            res.rows_affected() > 0,
            "no matching workspace lock held by {role_execution_id}"
        );
        Ok(())
    }

    /// Check if the active workspace mutation lock is held by .
    pub async fn check_workspace_mutation_lock(
        &self,
        attempt_id: &str,
        role_execution_id: &str,
    ) -> Result<bool> {
        let row = sqlx::query_scalar::<_, String>(
            "SELECT holder_role_execution_id FROM orbit_attempt_workspace_locks WHERE attempt_id = $1 AND revoked_at_ms IS NULL",
        )
        .bind(attempt_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.as_deref() == Some(role_execution_id))
    }

    /// Create and persist a new RoleExecution.
    pub async fn create_role_execution(
        &self,
        workflow_run_id: &str,
        role: &RoleDefinition,
        stage: &str,
        iteration: u32,
        input_workspace_state_id: Option<&str>,
        handoff_input_id: Option<&str>,
    ) -> Result<RoleExecution> {
        let re_id = format!("re-{}", id());
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as i64;

        let inserted = sqlx::query(
            r#"
            INSERT INTO orbit_role_executions (
                id, workflow_run_id, role_id, role_version, role_digest,
                stage, iteration, status, input_workspace_state_id,
                handoff_input_id, started_at_ms
            ) SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11
              FROM orbit_workflow_runs wf
              WHERE wf.id = $2 AND wf.status NOT IN ('COMPLETED', 'FAILED', 'CANCELLED', 'EXHAUSTED')
                AND (($12::text IS NULL AND wf.step_owner_id IS NULL)
                     OR (wf.step_owner_id = $12 AND wf.step_generation = $13))
              FOR UPDATE OF wf
            "#,
        )
        .bind(&re_id)
        .bind(workflow_run_id)
        .bind(&role.role_id)
        .bind(role.version as i32)
        .bind(role.digest())
        .bind(stage)
        .bind(iteration as i32)
        .bind(RoleExecutionStatus::Pending.as_str())
        .bind(input_workspace_state_id)
        .bind(handoff_input_id)
        .bind(now_ms)
        .bind(self.step_claim.as_ref().map(|claim| claim.owner_id.as_str()))
        .bind(self.step_claim.as_ref().map(|claim| claim.generation))
        .execute(&self.pool)
        .await
        .context("insert orbit_role_executions")?;
        ensure!(
            inserted.rows_affected() == 1,
            "ROLE_EXECUTION_FENCE_REJECTED"
        );

        self.get_role_execution(&re_id)
            .await?
            .context("role execution not found after insert")
    }

    pub async fn get_role_execution(&self, re_id: &str) -> Result<Option<RoleExecution>> {
        let row = sqlx::query(
            r#"
            SELECT id, workflow_run_id, role_id, role_version, role_digest,
                   stage, iteration, status, input_workspace_state_id,
                   output_workspace_state_id, resolved_target, agent_execution_ids,
                   handoff_input_id, handoff_output_id, started_at_ms, finished_at_ms,
                   termination_reason, failure_message
            FROM orbit_role_executions WHERE id = $1
            "#,
        )
        .bind(re_id)
        .fetch_optional(&self.pool)
        .await
        .context("select orbit_role_executions")?;

        let Some(r) = row else { return Ok(None) };
        let status_str: String = r.get("status");
        let status = RoleExecutionStatus::from_str_strict(&status_str)?;
        let resolved_val: Option<serde_json::Value> = r.get("resolved_target");
        let resolved_target = match resolved_val {
            Some(v) => Some(serde_json::from_value(v)?),
            None => None,
        };
        let agent_ids_val: serde_json::Value = r.get("agent_execution_ids");
        let agent_execution_ids: Vec<String> = serde_json::from_value(agent_ids_val)?;

        Ok(Some(RoleExecution {
            id: r.get("id"),
            workflow_run_id: r.get("workflow_run_id"),
            role_id: r.get("role_id"),
            role_version: r.get::<i32, _>("role_version") as u32,
            role_digest: r.get("role_digest"),
            stage: r.get("stage"),
            iteration: r.get::<i32, _>("iteration") as u32,
            status,
            input_workspace_state_id: r.get("input_workspace_state_id"),
            output_workspace_state_id: r.get("output_workspace_state_id"),
            resolved_target,
            agent_execution_ids,
            handoff_input_id: r.get("handoff_input_id"),
            handoff_output_id: r.get("handoff_output_id"),
            started_at_ms: r.get("started_at_ms"),
            finished_at_ms: r.get("finished_at_ms"),
            termination_reason: r.get("termination_reason"),
            failure_message: r.get("failure_message"),
        }))
    }

    pub async fn list_role_executions(&self, wf_id: &str) -> Result<Vec<RoleExecution>> {
        let rows = sqlx::query(
            r#"
            SELECT id, workflow_run_id, role_id, role_version, role_digest,
                   stage, iteration, status, input_workspace_state_id,
                   output_workspace_state_id, resolved_target, agent_execution_ids,
                   handoff_input_id, handoff_output_id, started_at_ms, finished_at_ms,
                   termination_reason, failure_message
            FROM orbit_role_executions
            WHERE workflow_run_id = $1
            ORDER BY started_at_ms ASC
            "#,
        )
        .bind(wf_id)
        .fetch_all(&self.pool)
        .await
        .context("list orbit_role_executions")?;

        let mut out = Vec::new();
        for r in rows {
            let status_str: String = r.get("status");
            let status = RoleExecutionStatus::from_str_strict(&status_str)?;
            let resolved_val: Option<serde_json::Value> = r.get("resolved_target");
            let resolved_target = match resolved_val {
                Some(v) => Some(serde_json::from_value(v)?),
                None => None,
            };
            let agent_ids_val: serde_json::Value = r.get("agent_execution_ids");
            let agent_execution_ids: Vec<String> = serde_json::from_value(agent_ids_val)?;

            out.push(RoleExecution {
                id: r.get("id"),
                workflow_run_id: r.get("workflow_run_id"),
                role_id: r.get("role_id"),
                role_version: r.get::<i32, _>("role_version") as u32,
                role_digest: r.get("role_digest"),
                stage: r.get("stage"),
                iteration: r.get::<i32, _>("iteration") as u32,
                status,
                input_workspace_state_id: r.get("input_workspace_state_id"),
                output_workspace_state_id: r.get("output_workspace_state_id"),
                resolved_target,
                agent_execution_ids,
                handoff_input_id: r.get("handoff_input_id"),
                handoff_output_id: r.get("handoff_output_id"),
                started_at_ms: r.get("started_at_ms"),
                finished_at_ms: r.get("finished_at_ms"),
                termination_reason: r.get("termination_reason"),
                failure_message: r.get("failure_message"),
            });
        }
        Ok(out)
    }

    /// Update role execution with resolved runtime target.
    pub async fn set_role_execution_resolved(
        &self,
        re_id: &str,
        target: &ResolvedExecutionTarget,
    ) -> Result<()> {
        let val = serde_json::to_value(target)?;
        let result = sqlx::query(
            r#"
            UPDATE orbit_role_executions
            SET status = $1, resolved_target = $2
            WHERE id = $3 AND status IN ('PENDING', 'RUNNING')
              AND EXISTS (SELECT 1 FROM orbit_workflow_runs wf
                  WHERE wf.id = workflow_run_id
                    AND wf.status NOT IN ('COMPLETED', 'FAILED', 'CANCELLED', 'EXHAUSTED')
                    AND (($4::text IS NULL AND wf.step_owner_id IS NULL)
                         OR (wf.step_owner_id = $4 AND wf.step_generation = $5)))
            "#,
        )
        .bind(RoleExecutionStatus::Running.as_str())
        .bind(val)
        .bind(re_id)
        .bind(
            self.step_claim
                .as_ref()
                .map(|claim| claim.owner_id.as_str()),
        )
        .bind(self.step_claim.as_ref().map(|claim| claim.generation))
        .execute(&self.pool)
        .await
        .context("update orbit_role_executions resolved")?;
        ensure!(result.rows_affected() == 1, "ROLE_EXECUTION_FENCE_REJECTED");
        Ok(())
    }

    /// Record an agent execution id against the role execution (preserving all fallback attempts).
    pub async fn record_agent_execution(&self, re_id: &str, agent_exec_id: &str) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE orbit_role_executions
            SET agent_execution_ids = agent_execution_ids || jsonb_build_array($1::text)
            WHERE id = $2
            "#,
        )
        .bind(agent_exec_id)
        .bind(re_id)
        .execute(&self.pool)
        .await
        .context("append agent_execution_id")?;
        Ok(())
    }

    /// Insert durable agent execution record in orbit_agent_executions table.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_agent_execution(
        &self,
        id: &str,
        role_execution_id: &str,
        agent_type: &str,
        provider: Option<&str>,
        model: Option<&str>,
        started_at_ms: i64,
        finished_at_ms: Option<i64>,
        status: &str,
        termination_reason: Option<&str>,
        exit_code: Option<i32>,
        message: Option<&str>,
        requested_model: Option<&str>,
        resolved_model: Option<&str>,
        actual_model: Option<&str>,
        turn_count: i64,
        tool_call_count: i64,
        tool_success_count: i64,
        tool_failure_count: i64,
        tool_counts: &serde_json::Value,
        metadata: &serde_json::Value,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO orbit_agent_executions (
                id, role_execution_id, agent_type, provider, model,
                started_at_ms, finished_at_ms, status, termination_reason,
                exit_code, message, requested_model, resolved_model, actual_model,
                turn_count, tool_call_count, tool_success_count, tool_failure_count,
                tool_counts, metadata
            ) VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20
            )
            "#,
        )
        .bind(id)
        .bind(role_execution_id)
        .bind(agent_type)
        .bind(provider)
        .bind(model)
        .bind(started_at_ms)
        .bind(finished_at_ms)
        .bind(status)
        .bind(termination_reason)
        .bind(exit_code)
        .bind(message)
        .bind(requested_model)
        .bind(resolved_model)
        .bind(actual_model)
        .bind(turn_count)
        .bind(tool_call_count)
        .bind(tool_success_count)
        .bind(tool_failure_count)
        .bind(tool_counts)
        .bind(metadata)
        .execute(&self.pool)
        .await
        .context("insert orbit_agent_executions")?;
        Ok(())
    }

    /// Start a fenced ACP AgentExecution before provider prompt/tool dispatch.
    /// The AgentExecution row and role link are committed together so every
    /// subsequent tool audit update has a durable owner before effects begin.
    #[allow(clippy::too_many_arguments)]
    pub async fn start_agent_execution(
        &self,
        id: &str,
        role_execution_id: &str,
        agent_type: &str,
        provider: Option<&str>,
        model: Option<&str>,
        started_at_ms: i64,
        requested_model: Option<&str>,
        resolved_model: Option<&str>,
        metadata: &serde_json::Value,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await.context("begin agent execution")?;
        let inserted = sqlx::query(
            r#"
            INSERT INTO orbit_agent_executions (
                id, role_execution_id, agent_type, provider, model,
                started_at_ms, status, requested_model, resolved_model,
                turn_count, tool_call_count, tool_success_count, tool_failure_count,
                tool_counts, metadata
            ) SELECT $1, re.id, $3, $4, $5, $6, 'RUNNING', $7, $8,
                     0, 0, 0, 0, '{}'::jsonb, $9
              FROM orbit_role_executions re
              JOIN orbit_workflow_runs wf ON wf.id = re.workflow_run_id
              WHERE re.id = $2 AND re.status = 'RUNNING'
                AND wf.status NOT IN ('COMPLETED', 'FAILED', 'CANCELLED', 'EXHAUSTED')
            "#,
        )
        .bind(id)
        .bind(role_execution_id)
        .bind(agent_type)
        .bind(provider)
        .bind(model)
        .bind(started_at_ms)
        .bind(requested_model)
        .bind(resolved_model)
        .bind(metadata)
        .execute(&mut *tx)
        .await
        .context("start orbit_agent_executions")?;
        ensure!(
            inserted.rows_affected() == 1,
            "AGENT_EXECUTION_FENCE_REJECTED"
        );

        let linked = sqlx::query(
            r#"
            UPDATE orbit_role_executions
            SET agent_execution_ids = agent_execution_ids || jsonb_build_array($1::text)
            WHERE id = $2 AND status = 'RUNNING'
              AND EXISTS (SELECT 1 FROM orbit_agent_executions ae
                          WHERE ae.id = $1 AND ae.role_execution_id = $2)
            "#,
        )
        .bind(id)
        .bind(role_execution_id)
        .execute(&mut *tx)
        .await
        .context("link started agent execution")?;
        ensure!(
            linked.rows_affected() == 1,
            "AGENT_EXECUTION_FENCE_REJECTED"
        );
        tx.commit().await.context("commit agent execution start")?;
        Ok(())
    }

    /// Persist the bounded ACP callback audit while its exact role and agent
    /// executions remain active. This write is completed before a callback may
    /// enter repository authorization or mutation branches.
    pub async fn update_running_agent_tool_audit(
        &self,
        agent_execution_id: &str,
        role_execution_id: &str,
        tool_call_audit: &serde_json::Value,
    ) -> Result<()> {
        let updated = sqlx::query(
            r#"
            UPDATE orbit_agent_executions ae
            SET metadata = jsonb_set(
                jsonb_set(ae.metadata, '{tool_call_audit}', $3, true),
                '{lifecycle}',
                COALESCE(ae.metadata->'lifecycle', '{}'::jsonb) || jsonb_build_object(
                    'phase', 'TOOL_ACTIVITY',
                    'tool_audit_applicability', 'APPLICABLE',
                    'milestones', CASE
                        WHEN COALESCE(ae.metadata->'lifecycle'->'milestones', '[]'::jsonb) @> '["TOOL_ACTIVITY_OBSERVED"]'::jsonb
                            THEN COALESCE(ae.metadata->'lifecycle'->'milestones', '[]'::jsonb)
                        WHEN jsonb_array_length(COALESCE(ae.metadata->'lifecycle'->'milestones', '[]'::jsonb)) < 16
                            THEN COALESCE(ae.metadata->'lifecycle'->'milestones', '[]'::jsonb) || '["TOOL_ACTIVITY_OBSERVED"]'::jsonb
                        ELSE COALESCE(ae.metadata->'lifecycle'->'milestones', '[]'::jsonb)
                    END
                ),
                true
            )
            WHERE ae.id = $1 AND ae.role_execution_id = $2 AND ae.status = 'RUNNING'
              AND EXISTS (SELECT 1 FROM orbit_role_executions re
                          JOIN orbit_workflow_runs wf ON wf.id = re.workflow_run_id
                          WHERE re.id = $2 AND re.status = 'RUNNING'
                            AND wf.status NOT IN ('COMPLETED', 'FAILED', 'CANCELLED', 'EXHAUSTED'))
            "#,
        )
        .bind(agent_execution_id)
        .bind(role_execution_id)
        .bind(tool_call_audit)
        .execute(&self.pool)
        .await
        .context("persist running ACP tool audit")?;
        ensure!(
            updated.rows_affected() == 1,
            "AGENT_EXECUTION_FENCE_REJECTED"
        );
        Ok(())
    }

    /// Persist one bounded ACP lifecycle snapshot while the exact role and
    /// workflow still authorize the active AgentExecution.
    pub async fn update_running_agent_execution_lifecycle(
        &self,
        agent_execution_id: &str,
        role_execution_id: &str,
        lifecycle: &serde_json::Value,
    ) -> Result<()> {
        let updated = sqlx::query(
            r#"
            UPDATE orbit_agent_executions ae
            SET metadata = jsonb_set(ae.metadata, '{lifecycle}', $3, true)
            WHERE ae.id = $1 AND ae.role_execution_id = $2 AND ae.status = 'RUNNING'
              AND EXISTS (SELECT 1 FROM orbit_role_executions re
                          JOIN orbit_workflow_runs wf ON wf.id = re.workflow_run_id
                          WHERE re.id = $2 AND re.status = 'RUNNING'
                            AND wf.status NOT IN ('COMPLETED', 'FAILED', 'CANCELLED', 'EXHAUSTED'))
            "#,
        )
        .bind(agent_execution_id)
        .bind(role_execution_id)
        .bind(lifecycle)
        .execute(&self.pool)
        .await
        .context("persist agent execution lifecycle")?;
        ensure!(
            updated.rows_affected() == 1,
            "AGENT_EXECUTION_FENCE_REJECTED"
        );
        Ok(())
    }

    /// Finish the same durable AgentExecution row that owned pre-effect audit
    /// writes. A missing row or changed role owner is a hard fence failure.
    #[allow(clippy::too_many_arguments)]
    pub async fn finish_agent_execution(
        &self,
        id: &str,
        role_execution_id: &str,
        finished_at_ms: i64,
        status: &str,
        termination_reason: Option<&str>,
        exit_code: Option<i32>,
        message: Option<&str>,
        requested_model: Option<&str>,
        resolved_model: Option<&str>,
        actual_model: Option<&str>,
        turn_count: i64,
        tool_call_count: i64,
        tool_success_count: i64,
        tool_failure_count: i64,
        tool_counts: &serde_json::Value,
        metadata: &serde_json::Value,
    ) -> Result<()> {
        let updated = sqlx::query(
            r#"
            UPDATE orbit_agent_executions ae
            SET finished_at_ms = $3, status = $4, termination_reason = $5,
                exit_code = $6, message = $7, actual_model = $8,
                requested_model = $9, resolved_model = $10,
                turn_count = $11, tool_call_count = $12, tool_success_count = $13,
                tool_failure_count = $14, tool_counts = $15, metadata = ae.metadata || $16
            WHERE ae.id = $1 AND ae.role_execution_id = $2 AND ae.status = 'RUNNING'
              AND EXISTS (SELECT 1 FROM orbit_role_executions re
                          JOIN orbit_workflow_runs wf ON wf.id = re.workflow_run_id
                          WHERE re.id = $2 AND re.status = 'RUNNING'
                            AND wf.status NOT IN ('COMPLETED', 'FAILED', 'CANCELLED', 'EXHAUSTED'))
            "#,
        )
        .bind(id)
        .bind(role_execution_id)
        .bind(finished_at_ms)
        .bind(status)
        .bind(termination_reason)
        .bind(exit_code)
        .bind(message)
        .bind(actual_model)
        .bind(requested_model)
        .bind(resolved_model)
        .bind(turn_count)
        .bind(tool_call_count)
        .bind(tool_success_count)
        .bind(tool_failure_count)
        .bind(tool_counts)
        .bind(metadata)
        .execute(&self.pool)
        .await
        .context("finish orbit_agent_executions")?;
        ensure!(
            updated.rows_affected() == 1,
            "AGENT_EXECUTION_FENCE_REJECTED"
        );
        Ok(())
    }

    /// Complete role execution with success and output handoff.
    pub async fn complete_role_execution_success(
        &self,
        re_id: &str,
        output_workspace_state_id: Option<&str>,
        handoff_output_id: Option<&str>,
    ) -> Result<RoleExecution> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as i64;

        let result = sqlx::query(
            r#"
            UPDATE orbit_role_executions
            SET status = $1,
                output_workspace_state_id = $2,
                handoff_output_id = $3,
                finished_at_ms = $4,
                termination_reason = 'success'
            WHERE id = $5 AND status IN ('PENDING', 'RUNNING')
              AND EXISTS (SELECT 1 FROM orbit_workflow_runs wf
                  WHERE wf.id = workflow_run_id
                    AND wf.status NOT IN ('COMPLETED', 'FAILED', 'CANCELLED', 'EXHAUSTED')
                    AND (($6::text IS NULL AND wf.step_owner_id IS NULL)
                         OR (wf.step_owner_id = $6 AND wf.step_generation = $7)))
            "#,
        )
        .bind(RoleExecutionStatus::Succeeded.as_str())
        .bind(output_workspace_state_id)
        .bind(handoff_output_id)
        .bind(now_ms)
        .bind(re_id)
        .bind(
            self.step_claim
                .as_ref()
                .map(|claim| claim.owner_id.as_str()),
        )
        .bind(self.step_claim.as_ref().map(|claim| claim.generation))
        .execute(&self.pool)
        .await
        .context("complete orbit_role_executions success")?;
        ensure!(result.rows_affected() == 1, "ROLE_EXECUTION_FENCE_REJECTED");

        self.get_role_execution(re_id)
            .await?
            .context("role execution not found")
    }

    /// Fail role execution with reason and diagnostic message.
    pub async fn complete_role_execution_failed(
        &self,
        re_id: &str,
        reason: &str,
        message: &str,
    ) -> Result<RoleExecution> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as i64;

        let result = sqlx::query(
            r#"
            UPDATE orbit_role_executions
            SET status = $1,
                finished_at_ms = $2,
                termination_reason = $3,
                failure_message = $4
            WHERE id = $5 AND status IN ('PENDING', 'RESOLVING', 'RUNNING')
              AND EXISTS (SELECT 1 FROM orbit_workflow_runs wf
                  WHERE wf.id = workflow_run_id
                    AND wf.status NOT IN ('COMPLETED', 'FAILED', 'CANCELLED', 'EXHAUSTED')
                    AND (($6::text IS NULL AND wf.step_owner_id IS NULL)
                         OR (wf.step_owner_id = $6 AND wf.step_generation = $7)))
            "#,
        )
        .bind(RoleExecutionStatus::Failed.as_str())
        .bind(now_ms)
        .bind(reason)
        .bind(message)
        .bind(re_id)
        .bind(
            self.step_claim
                .as_ref()
                .map(|claim| claim.owner_id.as_str()),
        )
        .bind(self.step_claim.as_ref().map(|claim| claim.generation))
        .execute(&self.pool)
        .await
        .context("complete orbit_role_executions failed")?;
        ensure!(result.rows_affected() == 1, "ROLE_EXECUTION_FENCE_REJECTED");

        self.get_role_execution(re_id)
            .await?
            .context("role execution not found")
    }

    /// Save a structured handoff artifact.
    pub async fn save_handoff_artifact(
        &self,
        workflow_run_id: &str,
        role_execution_id: Option<&str>,
        handoff_type: HandoffType,
        workspace_state_id: Option<&str>,
        structured_payload: serde_json::Value,
    ) -> Result<HandoffArtifact> {
        let hid = format!("ha-{}", id());
        let inserted = sqlx::query(
            r#"
            INSERT INTO orbit_handoff_artifacts (
                id, workflow_run_id, role_execution_id, handoff_type,
                version, workspace_state_id, structured_payload
            ) SELECT $1, $2, $3, $4, $5, $6, $7
              FROM orbit_workflow_runs wf
              WHERE wf.id = $2 AND wf.status NOT IN ('COMPLETED', 'FAILED', 'CANCELLED', 'EXHAUSTED')
                AND (($8::text IS NULL AND wf.step_owner_id IS NULL)
                     OR (wf.step_owner_id = $8 AND wf.step_generation = $9))
              FOR UPDATE OF wf
            "#,
        )
        .bind(&hid)
        .bind(workflow_run_id)
        .bind(role_execution_id)
        .bind(handoff_type.as_str())
        .bind(1)
        .bind(workspace_state_id)
        .bind(&structured_payload)
        .bind(self.step_claim.as_ref().map(|claim| claim.owner_id.as_str()))
        .bind(self.step_claim.as_ref().map(|claim| claim.generation))
        .execute(&self.pool)
        .await
        .context("insert orbit_handoff_artifacts")?;
        ensure!(inserted.rows_affected() == 1, "HANDOFF_FENCE_REJECTED");

        Ok(HandoffArtifact {
            id: hid,
            workflow_run_id: workflow_run_id.into(),
            role_execution_id: role_execution_id.map(Into::into),
            handoff_type,
            version: 1,
            workspace_state_id: workspace_state_id.map(Into::into),
            structured_payload,
        })
    }

    pub async fn get_handoff_artifact(&self, hid: &str) -> Result<Option<HandoffArtifact>> {
        let row = sqlx::query(
            r#"
            SELECT id, workflow_run_id, role_execution_id, handoff_type,
                   version, workspace_state_id, structured_payload
            FROM orbit_handoff_artifacts WHERE id = $1
            "#,
        )
        .bind(hid)
        .fetch_optional(&self.pool)
        .await
        .context("select orbit_handoff_artifacts")?;

        let Some(r) = row else { return Ok(None) };
        let type_str: String = r.get("handoff_type");
        let handoff_type = HandoffType::from_str_strict(&type_str)?;

        Ok(Some(HandoffArtifact {
            id: r.get("id"),
            workflow_run_id: r.get("workflow_run_id"),
            role_execution_id: r.get("role_execution_id"),
            handoff_type,
            version: r.get::<i32, _>("version") as u32,
            workspace_state_id: r.get("workspace_state_id"),
            structured_payload: r.get("structured_payload"),
        }))
    }

    pub async fn get_latest_handoff_of_type(
        &self,
        wf_id: &str,
        handoff_type: HandoffType,
    ) -> Result<Option<HandoffArtifact>> {
        let row = sqlx::query(
            r#"
            SELECT id, workflow_run_id, role_execution_id, handoff_type,
                   version, workspace_state_id, structured_payload
            FROM orbit_handoff_artifacts
            WHERE workflow_run_id = $1 AND handoff_type = $2
            ORDER BY created_at DESC
            LIMIT 1
            "#,
        )
        .bind(wf_id)
        .bind(handoff_type.as_str())
        .fetch_optional(&self.pool)
        .await
        .context("select latest orbit_handoff_artifacts")?;

        let Some(r) = row else { return Ok(None) };
        Ok(Some(HandoffArtifact {
            id: r.get("id"),
            workflow_run_id: r.get("workflow_run_id"),
            role_execution_id: r.get("role_execution_id"),
            handoff_type,
            version: r.get::<i32, _>("version") as u32,
            workspace_state_id: r.get("workspace_state_id"),
            structured_payload: r.get("structured_payload"),
        }))
    }

    /// Verify completion invariant:
    /// A workflow run may only be marked COMPLETED if:
    ///   - Current workspace state is Some(ws_id)
    ///   - The latest verification run on ws_id PASSED
    ///   - The latest review decision on ws_id was APPROVE
    ///   - The latest final regression run on ws_id PASSED
    ///   - No subsequent mutation has occurred on the workspace
    pub async fn check_completion_invariant(&self, wf_id: &str) -> Result<()> {
        let wf = self
            .get_workflow_run(wf_id)
            .await?
            .context("workflow run not found")?;

        let ws_id = wf
            .current_workspace_state_id
            .as_deref()
            .context("cannot complete workflow without workspace state")?;

        // 1. Check latest review handoff
        let review_artifact = self
            .get_latest_handoff_of_type(wf_id, HandoffType::Review)
            .await?
            .context("cannot complete workflow: no review handoff found")?;

        ensure!(
            review_artifact.workspace_state_id.as_deref() == Some(ws_id),
            "cannot complete workflow: review was performed on a different workspace state (stale review)"
        );

        let review_payload: ReviewDecision =
            serde_json::from_value(review_artifact.structured_payload)?;
        ensure!(
            review_payload.decision == ReviewDecisionStatus::Approve,
            "cannot complete workflow: review did not approve workspace (decision: {:?})",
            review_payload.decision
        );

        // 2. Check that verification evidence exists for ws_id
        let runs = self.verification_store.list_runs(&wf.attempt_id).await?;
        let matching_runs: Vec<_> = runs
            .into_iter()
            .filter(|r| {
                r.workspace_state_id == ws_id
                    && r.overall_result == Some(crate::verification::VerificationRunResult::Passed)
            })
            .collect();

        ensure!(
            !matching_runs.is_empty(),
            "cannot complete workflow: no passed verification evidence for workspace state {}",
            ws_id
        );

        // Completion requires a successful FULL regression tier.
        let required_tier = if let Some(ref reg_id) = wf.regression_policy_id {
            let reg_store = crate::regression_strategy::RegressionStore::new(self.pool.clone());
            let reg_pol = reg_store
                .get_regression_policy(reg_id, wf.regression_policy_version.unwrap_or(1))
                .await?;
            reg_pol
                .map(|p| p.completion_tier)
                .unwrap_or(crate::regression_strategy::VerificationTier::Full)
        } else {
            crate::regression_strategy::VerificationTier::Full
        };

        let has_passing_completion_tier = matching_runs.iter().any(|r| match r.tier {
            Some(t) => t >= required_tier,
            None => false,
        });

        let all_legacy =
            wf.regression_policy_id.is_none() && matching_runs.iter().all(|r| r.tier.is_none());
        ensure!(
            has_passing_completion_tier || all_legacy,
            "cannot complete workflow: workspace state {} has not qualified required completion tier {:?}",
            ws_id,
            required_tier
        );

        Ok(())
    }
}

/// Runtime resolver mapping RoleExecution requirements to a concrete execution target.
pub struct RoleRuntimeResolver;

/// Small, configurable safety thresholds used while ranking runtime targets.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeQuotaSelectionPolicy {
    #[serde(default = "default_min_5h_remaining_percent")]
    pub min_5h_remaining_percent: f64,
    #[serde(default = "default_min_7d_remaining_percent")]
    pub min_7d_remaining_percent: f64,
}

const fn default_min_5h_remaining_percent() -> f64 {
    15.0
}

const fn default_min_7d_remaining_percent() -> f64 {
    5.0
}

impl Default for RuntimeQuotaSelectionPolicy {
    fn default() -> Self {
        Self {
            min_5h_remaining_percent: default_min_5h_remaining_percent(),
            min_7d_remaining_percent: default_min_7d_remaining_percent(),
        }
    }
}

impl RuntimeQuotaSelectionPolicy {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.min_5h_remaining_percent.is_finite()
                && (0.0..=100.0).contains(&self.min_5h_remaining_percent),
            "invalid minimum 5h quota remaining percentage"
        );
        ensure!(
            self.min_7d_remaining_percent.is_finite()
                && (0.0..=100.0).contains(&self.min_7d_remaining_percent),
            "invalid minimum 7d quota remaining percentage"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct RuntimeQuotaFacts {
    five_hour_remaining: Option<f64>,
    seven_day_remaining: Option<f64>,
    seven_day_reset_at_ms: Option<i64>,
    explicitly_exhausted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QuotaSnapshotFreshness {
    Fresh,
    Stale,
    Absent,
}

impl QuotaSnapshotFreshness {
    fn as_evidence(self) -> &'static str {
        match self {
            Self::Fresh => "FRESH",
            Self::Stale => "STALE",
            Self::Absent => "ABSENT",
        }
    }
}

fn quota_snapshot_freshness(
    snapshot: Option<&crate::availability::AvailabilitySnapshot>,
    now_ms: i64,
) -> QuotaSnapshotFreshness {
    match snapshot {
        None => QuotaSnapshotFreshness::Absent,
        Some(snapshot) if snapshot.observed_at_ms <= now_ms && now_ms < snapshot.expires_at_ms => {
            QuotaSnapshotFreshness::Fresh
        }
        // A present snapshot outside its validity interval is evidence that a
        // snapshot exists, but its quota facts are not current and are unused.
        Some(_) => QuotaSnapshotFreshness::Stale,
    }
}

type NormalizedQuotaWindow = (Option<i64>, String, Option<f64>, Option<i64>, Option<bool>);

#[derive(Clone, Debug)]
struct RuntimeCandidate {
    target: ResolvedExecutionTarget,
    provider_preference_rank: usize,
    credential_reference: String,
    credential_id: String,
    quota: RuntimeQuotaFacts,
    tool_audit_capability: crate::acp_capabilities::ToolAuditCorrelationCapability,
    quota_snapshot_freshness: QuotaSnapshotFreshness,
    availability: crate::availability::AvailabilityState,
}

fn quota_percent(
    remaining_percent: Option<f64>,
    remaining_fraction: Option<f64>,
    used_percent: Option<f64>,
) -> Option<f64> {
    remaining_percent
        .or_else(|| remaining_fraction.map(|fraction| fraction * 100.0))
        .or_else(|| used_percent.map(|used| 100.0 - used))
        .filter(|percent| percent.is_finite() && (0.0..=100.0).contains(percent))
}

fn quota_window_kind(duration_minutes: Option<i64>, identifier: &str) -> Option<bool> {
    if duration_minutes == Some(300) {
        return Some(true);
    }
    if duration_minutes == Some(10_080) {
        return Some(false);
    }
    let identifier = identifier
        .rsplit('.')
        .next()
        .unwrap_or(identifier)
        .to_ascii_lowercase();
    match identifier.as_str() {
        "5h" | "5hr" | "5hours" => Some(true),
        "7d" | "weekly" | "1w" | "7days" => Some(false),
        _ => None,
    }
}

fn normalized_model_membership(value: &str) -> std::collections::BTreeSet<String> {
    value
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty() && !word.bytes().all(|byte| byte.is_ascii_digit()))
        .map(str::to_ascii_lowercase)
        .filter(|word| !matches!(word.as_str(), "high" | "low" | "medium" | "max" | "preview"))
        .collect()
}

fn is_codex_default_bucket(provider_label: Option<&str>) -> bool {
    provider_label.is_some_and(|label| label.eq_ignore_ascii_case("default"))
}

fn is_codex_default_flat_window(label: &str) -> bool {
    label
        .split('.')
        .next()
        .is_some_and(|bucket| bucket.eq_ignore_ascii_case("default"))
}

fn quota_facts_for_candidate(
    snapshot: Option<&crate::availability::AvailabilitySnapshot>,
    provider: &str,
    model: &str,
    now_ms: i64,
) -> RuntimeQuotaFacts {
    let Some(snapshot) = snapshot
        .filter(|snapshot| snapshot.observed_at_ms <= now_ms && now_ms < snapshot.expires_at_ms)
    else {
        return RuntimeQuotaFacts::default();
    };

    let mut windows: Vec<NormalizedQuotaWindow> = Vec::new();
    if !snapshot.quota_buckets.is_empty() {
        let applicable_buckets: std::collections::BTreeSet<&str> = if provider == "codex" {
            // The configured Luna target uses Codex's `default` bucket.
            // `gpt-reserve` must not supply its headroom or reset order.
            snapshot
                .quota_buckets
                .iter()
                .filter(|bucket| {
                    bucket.scope.is_none()
                        && is_codex_default_bucket(bucket.provider_label.as_deref())
                })
                .map(|bucket| bucket.provider_bucket_fingerprint.as_str())
                .collect()
        } else if snapshot.quota_groups.is_empty() {
            std::collections::BTreeSet::new()
        } else {
            let model_membership = normalized_model_membership(model);
            snapshot
                .quota_groups
                .iter()
                .filter(|group| {
                    group.members.iter().any(|member| {
                        normalized_model_membership(&member.provider_label) == model_membership
                    })
                })
                .flat_map(|group| group.bucket_fingerprints.iter().map(String::as_str))
                .collect()
        };

        for bucket in &snapshot.quota_buckets {
            if !applicable_buckets.contains(bucket.provider_bucket_fingerprint.as_str()) {
                continue;
            }
            for window in &bucket.windows {
                windows.push((
                    window.duration_minutes,
                    window.provider_window_id.clone(),
                    quota_percent(
                        window.remaining_percent,
                        window.remaining_fraction,
                        window.used_percent,
                    ),
                    window.resets_at_ms,
                    window.exhausted,
                ));
            }
        }
    } else {
        // Opaque historical Codex bucket hashes stay unknown rather than
        // being guessed to represent the selected model.
        windows.extend(
            snapshot
                .quota_windows
                .iter()
                .filter(|window| provider != "codex" || is_codex_default_flat_window(&window.label))
                .map(|window| {
                    (
                        window.duration_minutes,
                        window.label.clone(),
                        quota_percent(window.remaining_percent, None, window.used_percent),
                        window.resets_at_ms,
                        window.exhausted,
                    )
                }),
        );
    }

    let mut facts = RuntimeQuotaFacts::default();
    let mut short_values = Vec::new();
    let mut weekly_values = Vec::new();
    let mut weekly_resets = Vec::new();
    for (duration, identifier, remaining, reset_at_ms, exhausted) in windows {
        // A reset has passed: its old percentage describes the previous
        // window and is not current headroom for V1 selection.
        if reset_at_ms.is_some_and(|reset| reset <= now_ms) {
            continue;
        }
        facts.explicitly_exhausted |= exhausted == Some(true);
        let Some(is_short) = quota_window_kind(duration, &identifier) else {
            continue;
        };
        if is_short {
            short_values.extend(remaining);
        } else {
            if let Some(remaining) = remaining {
                weekly_values.push(remaining);
                weekly_resets.extend(reset_at_ms);
            }
        }
    }
    facts.five_hour_remaining = short_values.into_iter().reduce(f64::min);
    facts.seven_day_remaining = weekly_values.into_iter().reduce(f64::min);
    facts.seven_day_reset_at_ms = weekly_resets.into_iter().min();
    facts
}

fn availability_blocks_candidate(state: crate::availability::AvailabilityState) -> bool {
    matches!(
        state,
        crate::availability::AvailabilityState::Cooldown
            | crate::availability::AvailabilityState::RateLimited
            | crate::availability::AvailabilityState::QuotaExhausted
            | crate::availability::AvailabilityState::AuthFailed
            | crate::availability::AvailabilityState::RuntimeUnavailable
            | crate::availability::AvailabilityState::CapabilityMismatch
    )
}

fn candidate_rejection(
    availability: crate::availability::AvailabilityState,
    quota: RuntimeQuotaFacts,
    policy: RuntimeQuotaSelectionPolicy,
) -> Option<&'static str> {
    if availability_blocks_candidate(availability) {
        return Some("explicitly_blocked_availability");
    }
    if quota.explicitly_exhausted {
        return Some("explicitly_exhausted_quota_window");
    }
    if quota
        .five_hour_remaining
        .is_some_and(|remaining| remaining < policy.min_5h_remaining_percent)
    {
        return Some("below_min_5h_remaining");
    }
    if quota
        .seven_day_remaining
        .is_some_and(|remaining| remaining < policy.min_7d_remaining_percent)
    {
        return Some("below_min_7d_remaining");
    }
    None
}

fn credential_has_valid_runtime_representation(
    inspection: &crate::credential_registry::CredentialInspection,
) -> bool {
    let expected_interface = match inspection.credential.provider.as_str() {
        "codex" => crate::codex_credential_enrollment::CODEX_INTERFACE,
        "antigravity" => "acp",
        _ => return false,
    };
    inspection.credential.has_secret
        && inspection.representations.iter().any(|representation| {
            representation.current_generation
                && representation.generation == inspection.credential.generation
                && representation.interface == expected_interface
                && representation.state == crate::credential_registry::RepresentationState::Stored
                && representation.validation == "valid"
                && representation.has_secret
        })
}

fn tool_audit_capability_rejection(
    required: Option<crate::acp_capabilities::ToolAuditCorrelationCapability>,
    provided: crate::acp_capabilities::ToolAuditCorrelationCapability,
) -> Option<String> {
    required
        .filter(|required| !provided.satisfies(*required))
        .map(|required| {
            format!(
                "CAPABILITY_MISMATCH(tool_audit_correlation_required={}/provided={})",
                required.as_evidence(),
                provided.as_evidence()
            )
        })
}

fn sort_runtime_candidates(candidates: &mut [RuntimeCandidate]) {
    let reset_rank = |candidate: &RuntimeCandidate| match (
        candidate.quota.seven_day_remaining,
        candidate.quota.seven_day_reset_at_ms,
    ) {
        (Some(_), Some(reset)) => (0u8, reset),
        _ => (1u8, i64::MAX),
    };
    candidates.sort_by(|left, right| {
        reset_rank(left)
            .cmp(&reset_rank(right))
            .then_with(|| {
                left.provider_preference_rank
                    .cmp(&right.provider_preference_rank)
            })
            .then_with(|| left.credential_id.cmp(&right.credential_id))
            .then_with(|| left.credential_reference.cmp(&right.credential_reference))
    });
}

fn runtime_candidate_selection_reason(
    candidate: &RuntimeCandidate,
    rank: usize,
    rejected: &str,
) -> String {
    let weekly_reset_rank = if candidate.quota.seven_day_remaining.is_some()
        && candidate.quota.seven_day_reset_at_ms.is_some()
    {
        "known_weekly_reset"
    } else {
        "weekly_reset_unknown_or_not_applicable"
    };
    format!(
        "reset-aware rank={rank}; {weekly_reset_rank}; tool_audit_correlation={}; quota_snapshot_freshness={}; availability={:?}; 5h_remaining={}; 7d_remaining={}; 7d_reset_at_ms={}; provider_preference_rank={}; tie_break=provider_preference_then_stable_account_id; rejected={rejected}",
        candidate.tool_audit_capability.as_evidence(),
        candidate.quota_snapshot_freshness.as_evidence(),
        candidate.availability,
        format_quota_percent(candidate.quota.five_hour_remaining),
        format_quota_percent(candidate.quota.seven_day_remaining),
        candidate
            .quota
            .seven_day_reset_at_ms
            .map(|reset| reset.to_string())
            .unwrap_or_else(|| "unknown".into()),
        candidate.provider_preference_rank
    )
}

fn format_quota_percent(percent: Option<f64>) -> String {
    percent
        .map(|value| format!("{value:.1}%"))
        .unwrap_or_else(|| "unknown".into())
}

fn summarize_candidate_diagnostics(rejected: &[String]) -> String {
    const MAX_DIAGNOSTICS: usize = 8;
    let displayed = rejected
        .iter()
        .take(MAX_DIAGNOSTICS)
        .cloned()
        .collect::<Vec<_>>();
    let omitted = rejected.len().saturating_sub(displayed.len());
    if omitted == 0 {
        format!("[{}]", displayed.join(","))
    } else {
        format!("[{},+{omitted}_omitted]", displayed.join(","))
    }
}

impl RoleRuntimeResolver {
    /// Resolve the first target using the default reset-aware quota policy.
    pub async fn resolve_target_live(
        pool: &sqlx::PgPool,
        role: &RoleDefinition,
        simulate_quota_exhausted_for: Option<&str>,
    ) -> Result<ResolvedExecutionTarget> {
        Self::resolve_target_live_with_policy(
            pool,
            role,
            simulate_quota_exhausted_for,
            RuntimeQuotaSelectionPolicy::default(),
        )
        .await
    }

    pub async fn resolve_target_live_with_policy(
        pool: &sqlx::PgPool,
        role: &RoleDefinition,
        simulate_quota_exhausted_for: Option<&str>,
        policy: RuntimeQuotaSelectionPolicy,
    ) -> Result<ResolvedExecutionTarget> {
        Self::resolve_ranked_targets_live(pool, role, simulate_quota_exhausted_for, policy)
            .await?
            .into_iter()
            .next()
            .context("runtime candidate ranking returned no eligible target")
    }

    /// Return every currently eligible target in deterministic selection order.
    /// This leaves continuation policy to the caller while giving retries the
    /// same ranked candidates in the same order.
    pub async fn resolve_ranked_targets_live(
        pool: &sqlx::PgPool,
        role: &RoleDefinition,
        simulate_quota_exhausted_for: Option<&str>,
        policy: RuntimeQuotaSelectionPolicy,
    ) -> Result<Vec<ResolvedExecutionTarget>> {
        policy.validate()?;
        let cred_store = crate::credential_registry::CredentialStore::new(pool);
        let credentials = cred_store.list().await?;
        let availability_store = crate::availability::AvailabilityStore::new(pool);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as i64;
        let mut candidates = Vec::new();
        let mut rejected = Vec::new();

        for (preference_rank, pref) in role.runtime_preferences.iter().enumerate() {
            if simulate_quota_exhausted_for == Some(pref.as_str()) {
                rejected.push(format!("preference={pref}:simulated_quota_exhausted"));
                continue;
            }

            let (provider, runtime_interface, model, runtime_image_digest, adapter_revision) =
                if pref.contains("codex") {
                    (
                        "codex",
                        "codex-acp",
                        "gpt-6-luna",
                        crate::codex_credential_enrollment::CODEX_IMAGE_DIGEST,
                        crate::codex_bridge::REVISION,
                    )
                } else if pref.contains("antigravity") {
                    (
                        "antigravity",
                        "antigravity-acp",
                        "gemini-3.8-flash",
                        crate::credential_enrollment::ANTIGRAVITY_DIGEST,
                        crate::acp_capabilities::ANTIGRAVITY_ACP_ADAPTER_REVISION,
                    )
                } else {
                    continue;
                };

            for credential in credentials.iter().filter(|credential| {
                credential.provider == provider
                    && credential.status == crate::credential_registry::CredentialStatus::Enrolled
            }) {
                let inspection = cred_store
                    .inspect(&credential.reference)
                    .await?
                    .context("credential disappeared during runtime resolution")?;
                if inspection.credential.generation != credential.generation
                    || inspection.credential.status
                        != crate::credential_registry::CredentialStatus::Enrolled
                {
                    rejected.push(format!(
                        "{provider}:{}:credential_changed_during_resolution",
                        credential.reference
                    ));
                    continue;
                }
                if !credential_has_valid_runtime_representation(&inspection) {
                    rejected.push(format!(
                        "{provider}:{}:missing_or_invalid_current_runtime_representation",
                        credential.reference
                    ));
                    continue;
                }

                let tool_audit_capability =
                    crate::acp_capabilities::qualified_tool_audit_correlation(
                        runtime_image_digest,
                        adapter_revision,
                    );
                if let Some(reason) = tool_audit_capability_rejection(
                    role.allowed_capabilities.required_tool_audit_correlation,
                    tool_audit_capability,
                ) {
                    rejected.push(format!(
                        "{runtime_interface}:{}:{reason}",
                        credential.reference
                    ));
                    continue;
                }

                let snapshot = availability_store
                    .current_for_credential(&credential.identity())
                    .await?;
                let snapshot_freshness = quota_snapshot_freshness(snapshot.as_ref(), now_ms);
                let snapshot_is_fresh = snapshot_freshness == QuotaSnapshotFreshness::Fresh;
                let availability = if snapshot_is_fresh {
                    snapshot.as_ref().unwrap().state
                } else {
                    crate::availability::AvailabilityState::Unknown
                };
                let quota = quota_facts_for_candidate(
                    snapshot.as_ref().filter(|_| snapshot_is_fresh),
                    provider,
                    model,
                    now_ms,
                );
                if let Some(reason) = candidate_rejection(availability, quota, policy) {
                    rejected.push(format!(
                        "{provider}:{}:{reason}(5h={},7d={},7d_reset={})",
                        credential.reference,
                        format_quota_percent(quota.five_hour_remaining),
                        format_quota_percent(quota.seven_day_remaining),
                        quota
                            .seven_day_reset_at_ms
                            .map(|reset| reset.to_string())
                            .unwrap_or_else(|| "unknown".into())
                    ));
                    continue;
                }

                let generation = u32::try_from(credential.generation)
                    .context("credential generation exceeds runtime target range")?;
                candidates.push(RuntimeCandidate {
                    target: ResolvedExecutionTarget {
                        provider: provider.into(),
                        runtime_interface: runtime_interface.into(),
                        credential_id: Some(credential.reference.clone()),
                        credential_generation: Some(generation),
                        requested_model: Some(model.into()),
                        resolved_model: Some(model.into()),
                        runtime_image_digest: Some(runtime_image_digest.into()),
                        resolution_reason: String::new(),
                    },
                    provider_preference_rank: preference_rank,
                    credential_reference: credential.reference.clone(),
                    credential_id: credential.id.clone(),
                    quota,
                    quota_snapshot_freshness: snapshot_freshness,
                    availability,
                    tool_audit_capability,
                });
            }
        }

        sort_runtime_candidates(&mut candidates);

        if candidates.is_empty() {
            let rejected = summarize_candidate_diagnostics(&rejected);
            bail!(
                "failed to resolve live execution target for role {}: all preferences exhausted or no eligible credentials enrolled in CredentialStore; rejected={rejected}",
                role.role_id
            );
        }

        let rejected = summarize_candidate_diagnostics(&rejected);
        Ok(candidates
            .into_iter()
            .enumerate()
            .map(|(rank, mut candidate)| {
                candidate.target.resolution_reason =
                    runtime_candidate_selection_reason(&candidate, rank + 1, &rejected);
                candidate.target
            })
            .collect())
    }

    pub fn resolve_target(
        role: &RoleDefinition,
        simulate_quota_exhausted_for: Option<&str>,
    ) -> Result<ResolvedExecutionTarget> {
        for pref in &role.runtime_preferences {
            if simulate_quota_exhausted_for == Some(pref.as_str()) {
                continue; // Skip exhausted runtime, fall back to next preference
            }

            let (provider, runtime) = if pref.contains("codex") {
                ("codex".to_string(), "codex-acp".to_string())
            } else if pref.contains("antigravity") {
                ("antigravity".to_string(), "antigravity-acp".to_string())
            } else {
                ("local".to_string(), pref.clone())
            };

            return Ok(ResolvedExecutionTarget {
                provider,
                runtime_interface: runtime,
                credential_id: Some(format!("cred-{}", role.role_id)),
                credential_generation: Some(1),
                requested_model: Some("auto".to_string()),
                resolved_model: Some(if pref.contains("codex") {
                    "gpt-5-codex".to_string()
                } else {
                    "gemini-2.5-pro".to_string()
                }),
                runtime_image_digest: None,
                resolution_reason: format!("resolved by preference '{}'", pref),
            });
        }

        bail!(
            "failed to resolve execution target for role '{}': all preferences exhausted",
            role.role_id
        )
    }
}

/// Human readable formatting of a WorkflowRun.
pub fn format_workflow_show(wf: &WorkflowRun, roles: &[RoleExecution]) -> String {
    let mut out = String::new();
    out.push_str(&format!("Workflow  {}\n", wf.id));
    out.push_str(&format!(
        "Kind      {}@{}\n",
        wf.workflow_kind, wf.workflow_version
    ));
    out.push_str(&format!("Attempt   {}\n", wf.attempt_id));
    out.push_str(&format!("Status    {}\n", wf.status.as_str()));
    out.push_str(&format!(
        "Iteration {}/{}\n",
        wf.iteration, wf.max_iterations
    ));
    out.push_str(&format!(
        "Workspace {}\n",
        wf.current_workspace_state_id.as_deref().unwrap_or("none")
    ));

    if let Some(r) = &wf.failure_reason {
        out.push_str(&format!("Failure   {}\n", r));
    }
    if let Some(c) = &wf.cancellation_reason {
        out.push_str(&format!("Cancelled {}\n", c));
    }

    out.push_str("\nSTAGES\n\n");
    for re in roles {
        out.push_str(&format!(
            "{} (iteration {})\n",
            re.stage.to_uppercase(),
            re.iteration
        ));
        out.push_str(&format!("  role:     {}@{}\n", re.role_id, re.role_version));
        out.push_str(&format!("  status:   {}\n", re.status.as_str()));
        if let Some(tgt) = &re.resolved_target {
            out.push_str(&format!(
                "  runtime:  {} ({})\n",
                tgt.runtime_interface, tgt.provider
            ));
            if let Some(m) = &tgt.resolved_model {
                out.push_str(&format!("  model:    {}\n", m));
            }
        }
        if let Some(ws) = &re.output_workspace_state_id {
            out.push_str(&format!("  workspace:{}\n", ws));
        }
        if !re.agent_execution_ids.is_empty() {
            out.push_str(&format!(
                "  agent_executions: {}\n",
                re.agent_execution_ids.join(", ")
            ));
        }
        if let Some(msg) = &re.failure_message {
            out.push_str(&format!("  error:    {}\n", msg));
        }
        out.push('\n');
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime_credential_inspection(
        provider: &str,
        interface: &str,
    ) -> crate::credential_registry::CredentialInspection {
        use crate::credential_registry::{
            CredentialInspection, CredentialStatus, CredentialView, RepresentationState,
            RepresentationView,
        };

        CredentialInspection {
            credential: CredentialView {
                id: "credential-id".into(),
                provider: provider.into(),
                reference: "fixture-account".into(),
                generation: 3,
                endpoint: None,
                auth_type: "fixture-auth".into(),
                secret_backend: "fixture-backend".into(),
                status: CredentialStatus::Enrolled,
                has_secret: true,
                created_at_ms: 1,
                updated_at_ms: 1,
            },
            generations: Vec::new(),
            representations: vec![RepresentationView {
                id: "representation-id".into(),
                generation: 3,
                current_generation: true,
                interface: interface.into(),
                auth_type: "fixture-auth".into(),
                state: RepresentationState::Stored,
                validation: "valid".into(),
                capabilities: Vec::new(),
                runtime_provenance: None,
                enrollment_stage: None,
                has_secret: true,
                last_validated_at_ms: Some(1),
                created_at_ms: 1,
                updated_at_ms: 1,
            }],
            identity_bindings: Vec::new(),
        }
    }

    #[test]
    fn runtime_credentials_require_their_provider_interface() {
        let codex = runtime_credential_inspection(
            "codex",
            crate::codex_credential_enrollment::CODEX_INTERFACE,
        );
        let antigravity = runtime_credential_inspection("antigravity", "acp");

        assert!(credential_has_valid_runtime_representation(&codex));
        assert!(credential_has_valid_runtime_representation(&antigravity));
        assert!(!credential_has_valid_runtime_representation(
            &runtime_credential_inspection("codex", "acp")
        ));
        assert!(!credential_has_valid_runtime_representation(
            &runtime_credential_inspection("antigravity", "codex")
        ));
    }

    #[test]
    fn runtime_credentials_reject_stale_invalid_and_secretless_representations() {
        use crate::credential_registry::RepresentationState;

        let mut stale_generation = runtime_credential_inspection(
            "codex",
            crate::codex_credential_enrollment::CODEX_INTERFACE,
        );
        stale_generation.representations[0].generation = 2;
        assert!(!credential_has_valid_runtime_representation(
            &stale_generation
        ));

        let mut non_current = runtime_credential_inspection(
            "codex",
            crate::codex_credential_enrollment::CODEX_INTERFACE,
        );
        non_current.representations[0].current_generation = false;
        assert!(!credential_has_valid_runtime_representation(&non_current));

        let mut invalid_state = runtime_credential_inspection(
            "codex",
            crate::codex_credential_enrollment::CODEX_INTERFACE,
        );
        invalid_state.representations[0].state = RepresentationState::Invalid;
        assert!(!credential_has_valid_runtime_representation(&invalid_state));

        let mut invalid_validation = runtime_credential_inspection(
            "codex",
            crate::codex_credential_enrollment::CODEX_INTERFACE,
        );
        invalid_validation.representations[0].validation = "invalid".into();
        assert!(!credential_has_valid_runtime_representation(
            &invalid_validation
        ));

        let mut secretless = runtime_credential_inspection(
            "codex",
            crate::codex_credential_enrollment::CODEX_INTERFACE,
        );
        secretless.representations[0].has_secret = false;
        assert!(!credential_has_valid_runtime_representation(&secretless));
        secretless.representations[0].has_secret = true;
        secretless.credential.has_secret = false;
        assert!(!credential_has_valid_runtime_representation(&secretless));
    }

    fn candidate(
        provider: &str,
        reference: &str,
        account_id: &str,
        provider_preference_rank: usize,
        quota: RuntimeQuotaFacts,
    ) -> RuntimeCandidate {
        RuntimeCandidate {
            target: ResolvedExecutionTarget {
                provider: provider.into(),
                runtime_interface: format!("{provider}-acp"),
                credential_id: Some(reference.into()),
                credential_generation: Some(1),
                requested_model: Some("test-model".into()),
                resolved_model: Some("test-model".into()),
                runtime_image_digest: Some("sha256:test".into()),
                resolution_reason: String::new(),
            },
            provider_preference_rank,
            credential_reference: reference.into(),
            credential_id: account_id.into(),
            quota,
            tool_audit_capability: crate::acp_capabilities::ToolAuditCorrelationCapability::Exact,
            quota_snapshot_freshness: QuotaSnapshotFreshness::Fresh,
            availability: crate::availability::AvailabilityState::Ready,
        }
    }

    fn quota_snapshot(
        provider: &str,
        now_ms: i64,
        quota_windows: Vec<crate::availability::QuotaWindow>,
    ) -> crate::availability::AvailabilitySnapshot {
        crate::availability::AvailabilitySnapshot {
            applies_to: crate::availability::AvailabilityScope::Credential(
                crate::availability::CredentialIdentity {
                    provider: provider.into(),
                    reference: "fixture-account".into(),
                    generation: "1".into(),
                    catalog_id: None,
                },
            ),
            observed_at_ms: now_ms - 100,
            expires_at_ms: now_ms + 10_000,
            state: crate::availability::AvailabilityState::Unknown,
            quota_windows,
            quota_buckets: Vec::new(),
            quota_groups: Vec::new(),
            source: crate::availability::EvidenceSource::ProviderNativeStatus,
            confidence: crate::availability::EvidenceConfidence::AuthoritativeNative,
            source_revision: "fixture".into(),
            evidence_digest: format!("sha256:{}", "a".repeat(64)),
            provider_observed_at_ms: None,
            provider_status_observation: None,
        }
    }

    #[test]
    fn test_role_definitions_and_digests() {
        let planner = RoleDefinition::planner_v1();
        let implementer = RoleDefinition::implementer_v1();
        let reviewer = RoleDefinition::reviewer_v1();

        assert_eq!(planner.workspace_access, WorkspaceAccess::ReadOnly);
        assert!(!planner.allowed_capabilities.repo_write);
        assert_eq!(implementer.workspace_access, WorkspaceAccess::ReadWrite);
        assert!(implementer.allowed_capabilities.repo_write);
        assert_eq!(reviewer.workspace_access, WorkspaceAccess::ReadOnly);
        assert!(!reviewer.allowed_capabilities.repo_write);
        for role in [&planner, &implementer, &reviewer] {
            assert_eq!(
                role.allowed_capabilities.required_tool_audit_correlation,
                Some(crate::acp_capabilities::ToolAuditCorrelationCapability::Exact)
            );
        }

        let d1 = planner.digest();
        let d2 = implementer.digest();
        let d3 = reviewer.digest();
        assert_ne!(d1, d2);
        assert_ne!(d2, d3);
    }

    #[test]
    fn tool_audit_capability_mismatch_is_explicit() {
        use crate::acp_capabilities::ToolAuditCorrelationCapability as Capability;

        assert!(
            tool_audit_capability_rejection(Some(Capability::Exact), Capability::Exact).is_none()
        );
        assert!(
            tool_audit_capability_rejection(Some(Capability::Partial), Capability::Partial)
                .is_none()
        );
        assert!(tool_audit_capability_rejection(None, Capability::Unknown).is_none());
        for provided in [Capability::Partial, Capability::None, Capability::Unknown] {
            let reason = tool_audit_capability_rejection(Some(Capability::Exact), provided)
                .expect("unqualified runtime must be excluded");
            assert!(reason.contains("CAPABILITY_MISMATCH"));
            assert!(reason.contains(provided.as_evidence()));
        }
    }

    #[test]
    fn eligible_exact_runtimes_keep_reset_order_and_preference_tie_break() {
        use crate::acp_capabilities::ToolAuditCorrelationCapability as Capability;

        let quota = RuntimeQuotaFacts {
            seven_day_remaining: Some(60.0),
            seven_day_reset_at_ms: Some(4_000),
            ..RuntimeQuotaFacts::default()
        };
        let mut early = candidate("runtime-a", "account-a", "id-a", 1, quota);
        early.quota.seven_day_reset_at_ms = Some(2_000);
        let late = candidate("runtime-b", "account-b", "id-b", 0, quota);
        let mut candidates = vec![late.clone(), early];
        candidates.retain(|candidate| {
            tool_audit_capability_rejection(
                Some(Capability::Exact),
                candidate.tool_audit_capability,
            )
            .is_none()
        });
        sort_runtime_candidates(&mut candidates);
        assert_eq!(candidates[0].credential_reference, "account-a");

        let mut tied = vec![late, candidate("runtime-c", "account-c", "id-c", 1, quota)];
        sort_runtime_candidates(&mut tied);
        assert_eq!(tied[0].credential_reference, "account-b");

        tied[0].tool_audit_capability = Capability::Partial;
        tied.retain(|candidate| {
            tool_audit_capability_rejection(
                Some(Capability::Exact),
                candidate.tool_audit_capability,
            )
            .is_none()
        });
        assert_eq!(tied.len(), 1);
        assert_eq!(tied[0].credential_reference, "account-c");
    }

    #[test]
    fn test_runtime_resolution_and_fallback() {
        let reviewer = RoleDefinition::reviewer_v1();
        // Normal preference should pick antigravity
        let tgt1 = RoleRuntimeResolver::resolve_target(&reviewer, None).unwrap();
        assert_eq!(tgt1.provider, "antigravity");
        assert_eq!(tgt1.runtime_interface, "antigravity-acp");

        // When antigravity is exhausted, fallback to codex
        let tgt2 = RoleRuntimeResolver::resolve_target(&reviewer, Some("antigravity-acp")).unwrap();
        assert_eq!(tgt2.provider, "codex");
        assert_eq!(tgt2.runtime_interface, "codex-acp");
    }

    #[test]
    fn reset_aware_candidate_policy_covers_thresholds_ranking_and_ties() {
        let policy = RuntimeQuotaSelectionPolicy::default();
        let early = candidate(
            "codex",
            "account-a",
            "id-a",
            1,
            RuntimeQuotaFacts {
                five_hour_remaining: Some(30.0),
                seven_day_remaining: Some(70.0),
                seven_day_reset_at_ms: Some(1_000),
                explicitly_exhausted: false,
            },
        );
        let later = candidate(
            "antigravity",
            "account-b",
            "id-b",
            0,
            RuntimeQuotaFacts {
                five_hour_remaining: Some(80.0),
                seven_day_remaining: Some(90.0),
                seven_day_reset_at_ms: Some(5_000),
                explicitly_exhausted: false,
            },
        );
        let mut ranked = vec![later.clone(), early.clone()];
        sort_runtime_candidates(&mut ranked);
        assert_eq!(ranked[0].credential_reference, "account-a");

        let low_short = RuntimeQuotaFacts {
            five_hour_remaining: Some(14.0),
            seven_day_remaining: Some(60.0),
            seven_day_reset_at_ms: Some(500),
            explicitly_exhausted: false,
        };
        assert_eq!(
            candidate_rejection(
                crate::availability::AvailabilityState::Ready,
                low_short,
                policy
            ),
            Some("below_min_5h_remaining")
        );
        assert_eq!(
            candidate_rejection(
                crate::availability::AvailabilityState::Ready,
                RuntimeQuotaFacts {
                    five_hour_remaining: Some(15.0),
                    ..RuntimeQuotaFacts::default()
                },
                policy
            ),
            None,
            "the 5h threshold is inclusive"
        );
        assert_eq!(
            candidate_rejection(
                crate::availability::AvailabilityState::Ready,
                RuntimeQuotaFacts {
                    five_hour_remaining: Some(80.0),
                    seven_day_remaining: Some(4.0),
                    ..RuntimeQuotaFacts::default()
                },
                policy
            ),
            Some("below_min_7d_remaining")
        );
        assert_eq!(
            candidate_rejection(
                crate::availability::AvailabilityState::Ready,
                RuntimeQuotaFacts {
                    seven_day_remaining: Some(5.0),
                    ..RuntimeQuotaFacts::default()
                },
                policy
            ),
            None,
            "the 7d reserve threshold is inclusive"
        );

        let mut rejected_early = vec![
            candidate("codex", "low-short-early", "id-c", 0, low_short),
            later.clone(),
        ];
        rejected_early.retain(|candidate| {
            candidate_rejection(candidate.availability, candidate.quota, policy).is_none()
        });
        sort_runtime_candidates(&mut rejected_early);
        assert_eq!(rejected_early[0].credential_reference, "account-b");

        let unknown_reset = candidate(
            "codex",
            "unknown-reset",
            "id-d",
            0,
            RuntimeQuotaFacts {
                five_hour_remaining: Some(20.0),
                seven_day_remaining: Some(40.0),
                seven_day_reset_at_ms: None,
                explicitly_exhausted: false,
            },
        );
        let mut known_beats_unknown = vec![unknown_reset.clone(), early.clone()];
        sort_runtime_candidates(&mut known_beats_unknown);
        assert_eq!(known_beats_unknown[0].credential_reference, "account-a");

        let unknown_weekly = candidate(
            "codex",
            "unknown-weekly",
            "id-e",
            0,
            RuntimeQuotaFacts {
                five_hour_remaining: Some(20.0),
                ..RuntimeQuotaFacts::default()
            },
        );
        assert_eq!(
            candidate_rejection(unknown_weekly.availability, unknown_weekly.quota, policy),
            None,
            "unknown or absent windows remain eligible under the existing policy"
        );
        let no_reset_codex = candidate(
            "codex",
            "tie-codex",
            "id-f",
            0,
            RuntimeQuotaFacts {
                seven_day_remaining: Some(30.0),
                ..RuntimeQuotaFacts::default()
            },
        );
        let no_reset_antigravity = candidate(
            "antigravity",
            "tie-antigravity",
            "id-g",
            1,
            RuntimeQuotaFacts {
                seven_day_remaining: Some(30.0),
                ..RuntimeQuotaFacts::default()
            },
        );
        let mut both_unknown = vec![no_reset_antigravity.clone(), no_reset_codex.clone()];
        sort_runtime_candidates(&mut both_unknown);
        assert_eq!(both_unknown[0].target.provider, "codex");

        let same_reset_antigravity = candidate(
            "antigravity",
            "same-reset-antigravity",
            "id-h",
            0,
            RuntimeQuotaFacts {
                seven_day_remaining: Some(30.0),
                seven_day_reset_at_ms: Some(3_000),
                ..RuntimeQuotaFacts::default()
            },
        );
        let same_reset_codex = candidate(
            "codex",
            "same-reset-codex",
            "id-i",
            1,
            RuntimeQuotaFacts {
                seven_day_remaining: Some(30.0),
                seven_day_reset_at_ms: Some(3_000),
                ..RuntimeQuotaFacts::default()
            },
        );
        let mut reviewer_tie = vec![same_reset_codex.clone(), same_reset_antigravity.clone()];
        sort_runtime_candidates(&mut reviewer_tie);
        assert_eq!(reviewer_tie[0].target.provider, "antigravity");

        let planner_codex = candidate("codex", "planner-codex", "id-j", 0, same_reset_codex.quota);
        let planner_antigravity = candidate(
            "antigravity",
            "planner-antigravity",
            "id-k",
            1,
            same_reset_antigravity.quota,
        );
        let mut planner_tie = vec![planner_antigravity, planner_codex];
        sort_runtime_candidates(&mut planner_tie);
        assert_eq!(planner_tie[0].target.provider, "codex");

        let stable_id_later_ref = candidate("codex", "codex-a", "id-z", 0, same_reset_codex.quota);
        let stable_id_first_ref = candidate("codex", "codex-z", "id-a", 0, same_reset_codex.quota);
        let mut account_tie = vec![stable_id_later_ref, stable_id_first_ref];
        sort_runtime_candidates(&mut account_tie);
        assert_eq!(account_tie[0].credential_reference, "codex-z");

        let mut same_provider_reset = vec![later, early];
        sort_runtime_candidates(&mut same_provider_reset);
        assert_eq!(same_provider_reset[0].credential_reference, "account-a");

        for blocked in [
            crate::availability::AvailabilityState::QuotaExhausted,
            crate::availability::AvailabilityState::RateLimited,
            crate::availability::AvailabilityState::AuthFailed,
            crate::availability::AvailabilityState::RuntimeUnavailable,
            crate::availability::AvailabilityState::CapabilityMismatch,
            crate::availability::AvailabilityState::Cooldown,
        ] {
            assert_eq!(
                candidate_rejection(blocked, same_reset_antigravity.quota, policy),
                Some("explicitly_blocked_availability")
            );
        }

        let invalid_policy = RuntimeQuotaSelectionPolicy {
            min_5h_remaining_percent: f64::NAN,
            ..policy
        };
        assert!(invalid_policy.validate().is_err());
    }

    #[test]
    fn reset_aware_quota_facts_reuse_fresh_normalized_windows() {
        let now_ms = 10_000;
        let snapshot = quota_snapshot(
            "codex",
            now_ms,
            vec![
                crate::availability::QuotaWindow {
                    label: "default.5h".into(),
                    duration_minutes: Some(300),
                    used_percent: None,
                    remaining_percent: Some(30.0),
                    resets_at_ms: Some(20_000),
                    exhausted: None,
                },
                crate::availability::QuotaWindow {
                    label: "default.weekly".into(),
                    duration_minutes: Some(10_080),
                    used_percent: None,
                    remaining_percent: Some(74.0),
                    resets_at_ms: Some(30_000),
                    exhausted: None,
                },
                crate::availability::QuotaWindow {
                    label: "gpt-reserve.weekly".into(),
                    duration_minutes: Some(10_080),
                    used_percent: None,
                    remaining_percent: Some(1.0),
                    resets_at_ms: Some(15_000),
                    exhausted: None,
                },
            ],
        );
        assert_eq!(
            quota_facts_for_candidate(Some(&snapshot), "codex", "gpt-6-luna", now_ms),
            RuntimeQuotaFacts {
                five_hour_remaining: Some(30.0),
                seven_day_remaining: Some(74.0),
                seven_day_reset_at_ms: Some(30_000),
                explicitly_exhausted: false,
            }
        );

        let stale = quota_snapshot(
            "codex",
            now_ms,
            vec![crate::availability::QuotaWindow {
                label: "default.5h".into(),
                duration_minutes: Some(300),
                used_percent: None,
                remaining_percent: Some(90.0),
                resets_at_ms: Some(9_000),
                exhausted: None,
            }],
        );
        assert_eq!(
            quota_facts_for_candidate(Some(&stale), "codex", "gpt-6-luna", now_ms),
            RuntimeQuotaFacts::default(),
            "past-reset or expired snapshot values are unknown, not current headroom"
        );
    }

    #[test]
    fn reset_aware_snapshot_freshness_is_evidence_only() {
        let now_ms = 10_000;
        let fresh = quota_snapshot("codex", now_ms, Vec::new());
        let stale = crate::availability::AvailabilitySnapshot {
            expires_at_ms: now_ms,
            ..fresh.clone()
        };
        let future_observation = crate::availability::AvailabilitySnapshot {
            observed_at_ms: now_ms + 1,
            ..fresh.clone()
        };

        assert_eq!(
            quota_snapshot_freshness(None, now_ms).as_evidence(),
            "ABSENT"
        );
        assert_eq!(
            quota_snapshot_freshness(Some(&stale), now_ms).as_evidence(),
            "STALE"
        );
        assert_eq!(
            quota_snapshot_freshness(Some(&future_observation), now_ms).as_evidence(),
            "STALE",
            "a present snapshot outside its validity interval is not current evidence"
        );
        assert_eq!(
            quota_snapshot_freshness(Some(&fresh), now_ms).as_evidence(),
            "FRESH"
        );

        let known_fresh = candidate(
            "antigravity",
            "known-fresh",
            "id-known",
            1,
            RuntimeQuotaFacts {
                seven_day_remaining: Some(45.0),
                seven_day_reset_at_ms: Some(20_000),
                ..RuntimeQuotaFacts::default()
            },
        );
        let mut unknown_stale = candidate(
            "codex",
            "unknown-stale",
            "id-stale",
            0,
            quota_facts_for_candidate(Some(&stale), "codex", "gpt-6-luna", now_ms),
        );
        unknown_stale.quota_snapshot_freshness = quota_snapshot_freshness(Some(&stale), now_ms);
        unknown_stale.availability = crate::availability::AvailabilityState::Unknown;
        let mut unknown_absent = candidate(
            "codex",
            "unknown-absent",
            "id-absent",
            0,
            quota_facts_for_candidate(None, "codex", "gpt-6-luna", now_ms),
        );
        unknown_absent.quota_snapshot_freshness = quota_snapshot_freshness(None, now_ms);
        unknown_absent.availability = crate::availability::AvailabilityState::Unknown;
        let stale_reason = runtime_candidate_selection_reason(&unknown_stale, 2, "[]");
        let absent_reason = runtime_candidate_selection_reason(&unknown_absent, 3, "[]");
        assert!(stale_reason.contains(
            "weekly_reset_unknown_or_not_applicable; tool_audit_correlation=EXACT; quota_snapshot_freshness=STALE; availability=Unknown; 5h_remaining=unknown; 7d_remaining=unknown; 7d_reset_at_ms=unknown"
        ));
        assert!(absent_reason.contains(
            "weekly_reset_unknown_or_not_applicable; tool_audit_correlation=EXACT; quota_snapshot_freshness=ABSENT; availability=Unknown; 5h_remaining=unknown; 7d_remaining=unknown; 7d_reset_at_ms=unknown"
        ));

        let mut ranked = vec![unknown_stale, known_fresh.clone(), unknown_absent];
        sort_runtime_candidates(&mut ranked);
        assert_eq!(
            ranked[0].credential_reference, known_fresh.credential_reference,
            "fresh known weekly reset keeps the V1 rank regardless of evidence label"
        );
        assert_eq!(
            ranked[1].quota_snapshot_freshness,
            QuotaSnapshotFreshness::Absent
        );
        assert_eq!(
            ranked[2].quota_snapshot_freshness,
            QuotaSnapshotFreshness::Stale
        );
        assert_eq!(
            ranked[1].availability,
            crate::availability::AvailabilityState::Unknown
        );
        assert_eq!(
            ranked[2].availability,
            crate::availability::AvailabilityState::Unknown
        );
        assert_eq!(ranked[1].quota, RuntimeQuotaFacts::default());
        assert_eq!(ranked[2].quota, RuntimeQuotaFacts::default());

        let same_facts = RuntimeQuotaFacts {
            five_hour_remaining: Some(55.0),
            seven_day_remaining: Some(65.0),
            seven_day_reset_at_ms: Some(30_000),
            explicitly_exhausted: false,
        };
        let mut first_fresh = candidate("codex", "first", "id-a", 0, same_facts);
        let mut second_stale = candidate("codex", "second", "id-b", 0, same_facts);
        second_stale.quota_snapshot_freshness = QuotaSnapshotFreshness::Stale;
        let mut original_order = vec![first_fresh.clone(), second_stale.clone()];
        sort_runtime_candidates(&mut original_order);
        let serialize_rank = |candidates: &[RuntimeCandidate]| {
            let ordered_accounts: Vec<_> = candidates
                .iter()
                .map(|candidate| {
                    (
                        candidate.target.provider.as_str(),
                        candidate.credential_reference.as_str(),
                        candidate.credential_id.as_str(),
                    )
                })
                .collect();
            serde_json::to_vec(&ordered_accounts).unwrap()
        };
        let original_rank = serialize_rank(&original_order);

        first_fresh.quota_snapshot_freshness = QuotaSnapshotFreshness::Absent;
        second_stale.quota_snapshot_freshness = QuotaSnapshotFreshness::Fresh;
        let mut evidence_changed_order = vec![second_stale, first_fresh];
        sort_runtime_candidates(&mut evidence_changed_order);
        let evidence_changed_rank = serialize_rank(&evidence_changed_order);
        assert_eq!(evidence_changed_rank, original_rank);
    }

    #[test]
    fn codex_quota_selection_uses_default_and_ignores_gpt_reserve() {
        let now_ms = 10_000;
        let window = |provider_window_id: &str,
                      duration_minutes: i64,
                      remaining_percent: f64,
                      resets_at_ms: i64| {
            crate::availability::QuotaBucketWindow {
                provider_window_id: provider_window_id.into(),
                duration_minutes: Some(duration_minutes),
                used_percent: Some(100.0 - remaining_percent),
                remaining_percent: Some(remaining_percent),
                remaining_fraction: None,
                resets_at_ms: Some(resets_at_ms),
                provider_reset_time: None,
                exhausted: None,
            }
        };
        let bucket = |fingerprint: &str,
                      provider_label: &str,
                      five_hour_remaining: f64,
                      weekly_remaining: f64,
                      weekly_reset_at_ms: i64| {
            crate::availability::QuotaBucket {
                provider_bucket_fingerprint: format!("qb1:{fingerprint}"),
                provider_label: Some(provider_label.into()),
                scope: None,
                windows: vec![
                    window("primary", 300, five_hour_remaining, now_ms + 20_000),
                    window("secondary", 10_080, weekly_remaining, weekly_reset_at_ms),
                ],
            }
        };
        let mut snapshot = quota_snapshot("codex", now_ms, Vec::new());
        snapshot.quota_buckets = vec![
            bucket(&"a".repeat(64), "default", 30.0, 74.0, now_ms + 30_000),
            bucket(&"b".repeat(64), "gpt-reserve", 1.0, 1.0, now_ms + 1_000),
        ];

        let quota = quota_facts_for_candidate(Some(&snapshot), "codex", "gpt-6-luna", now_ms);
        assert_eq!(
            quota,
            RuntimeQuotaFacts {
                five_hour_remaining: Some(30.0),
                seven_day_remaining: Some(74.0),
                seven_day_reset_at_ms: Some(now_ms + 30_000),
                explicitly_exhausted: false,
            }
        );
        assert_eq!(
            candidate_rejection(
                crate::availability::AvailabilityState::Ready,
                quota,
                RuntimeQuotaSelectionPolicy::default()
            ),
            None,
            "the low, earlier-reset gpt-reserve bucket does not apply to the Luna target"
        );
    }

    #[test]
    fn antigravity_quota_selection_uses_the_matching_provider_model_group() {
        let now_ms = 10_000;
        let gemini_bucket = "qb1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let claude_bucket = "qb1:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let snapshot = crate::availability::AvailabilitySnapshot {
            applies_to: crate::availability::AvailabilityScope::Credential(
                crate::availability::CredentialIdentity {
                    provider: "antigravity".into(),
                    reference: "fixture-account".into(),
                    generation: "1".into(),
                    catalog_id: None,
                },
            ),
            observed_at_ms: now_ms - 100,
            expires_at_ms: now_ms + 10_000,
            state: crate::availability::AvailabilityState::Unknown,
            quota_windows: Vec::new(),
            quota_buckets: vec![
                crate::availability::QuotaBucket {
                    provider_bucket_fingerprint: gemini_bucket.into(),
                    provider_label: None,
                    scope: None,
                    windows: vec![
                        crate::availability::QuotaBucketWindow {
                            provider_window_id: "5h".into(),
                            duration_minutes: None,
                            used_percent: None,
                            remaining_percent: None,
                            remaining_fraction: Some(0.30),
                            resets_at_ms: Some(20_000),
                            provider_reset_time: None,
                            exhausted: None,
                        },
                        crate::availability::QuotaBucketWindow {
                            provider_window_id: "weekly".into(),
                            duration_minutes: None,
                            used_percent: None,
                            remaining_percent: None,
                            remaining_fraction: Some(0.70),
                            resets_at_ms: Some(30_000),
                            provider_reset_time: None,
                            exhausted: None,
                        },
                    ],
                },
                crate::availability::QuotaBucket {
                    provider_bucket_fingerprint: claude_bucket.into(),
                    provider_label: None,
                    scope: None,
                    windows: vec![crate::availability::QuotaBucketWindow {
                        provider_window_id: "weekly".into(),
                        duration_minutes: None,
                        used_percent: None,
                        remaining_percent: Some(1.0),
                        remaining_fraction: None,
                        resets_at_ms: Some(11_000),
                        provider_reset_time: None,
                        exhausted: None,
                    }],
                },
            ],
            quota_groups: vec![
                crate::availability::ProviderQuotaGroup {
                    fingerprint: format!("qg1:{}", "c".repeat(64)),
                    identity_basis: crate::availability::ProviderQuotaGroupIdentityBasis::MemberSet,
                    provider_display_name: Some("Gemini Models".into()),
                    provider_description: None,
                    members: vec![crate::availability::ProviderQuotaMember {
                        provider_label: "Gemini Flash".into(),
                        provider_key_fingerprint: None,
                    }],
                    bucket_fingerprints: vec![gemini_bucket.into()],
                },
                crate::availability::ProviderQuotaGroup {
                    fingerprint: format!("qg1:{}", "d".repeat(64)),
                    identity_basis: crate::availability::ProviderQuotaGroupIdentityBasis::MemberSet,
                    provider_display_name: Some("Claude and GPT models".into()),
                    provider_description: None,
                    members: vec![crate::availability::ProviderQuotaMember {
                        provider_label: "Claude Sonnet".into(),
                        provider_key_fingerprint: None,
                    }],
                    bucket_fingerprints: vec![claude_bucket.into()],
                },
            ],
            source: crate::availability::EvidenceSource::ProviderNativeStatus,
            confidence: crate::availability::EvidenceConfidence::AuthoritativeNative,
            source_revision: "fixture".into(),
            evidence_digest: format!("sha256:{}", "a".repeat(64)),
            provider_observed_at_ms: None,
            provider_status_observation: None,
        };
        assert_eq!(
            quota_facts_for_candidate(Some(&snapshot), "antigravity", "gemini-3.8-flash", now_ms),
            RuntimeQuotaFacts {
                five_hour_remaining: Some(30.0),
                seven_day_remaining: Some(70.0),
                seven_day_reset_at_ms: Some(30_000),
                explicitly_exhausted: false,
            }
        );
    }

    #[test]
    fn test_workflow_stage_parsing() {
        assert_eq!(
            WorkflowStage::from_str_strict("PLANNING").unwrap(),
            WorkflowStage::Planning
        );
        assert_eq!(
            WorkflowStage::from_str_strict("COMPLETED").unwrap(),
            WorkflowStage::Completed
        );
        assert!(WorkflowStage::Completed.is_terminal());
        assert!(WorkflowStage::Exhausted.is_terminal());
        assert!(!WorkflowStage::Verifying.is_terminal());
    }
}
