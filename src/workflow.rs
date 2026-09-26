//! Phase B3: Role Agents + Verified Sequential Workflow.
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
}

impl WorkflowStore {
    pub fn new(pool: PgPool) -> Self {
        let verification_store = VerificationStore::new(pool.clone());
        Self {
            pool,
            verification_store,
        }
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
        .bind(repository_path)
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

        sqlx::query(
            r#"
            UPDATE orbit_workflow_runs
            SET status = $1,
                current_stage = $2,
                current_workspace_state_id = COALESCE($3, current_workspace_state_id),
                iteration = COALESCE($4, iteration),
                finished_at_ms = COALESCE($5, finished_at_ms),
                failure_reason = COALESCE($6, failure_reason),
                cancellation_reason = COALESCE($7, cancellation_reason)
            WHERE id = $8
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
        .execute(&self.pool)
        .await
        .context("update orbit_workflow_runs status")?;

        self.get_workflow_run(wf_id)
            .await?
            .context("workflow run not found after update")
    }

    /// Acquire the exclusive workspace mutation lock for an attempt.
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
            INSERT INTO orbit_attempt_workspace_locks (attempt_id, holder_role_execution_id, acquired_at_ms)
            VALUES ($1, $2, $3)
            "#,
        )
        .bind(attempt_id)
        .bind(role_execution_id)
        .bind(now_ms)
        .execute(&self.pool)
        .await;

        match res {
            Ok(_) => Ok(()),
            Err(e) => {
                bail!("workspace mutation lock already held for attempt '{attempt_id}': {e}");
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

        sqlx::query(
            r#"
            INSERT INTO orbit_role_executions (
                id, workflow_run_id, role_id, role_version, role_digest,
                stage, iteration, status, input_workspace_state_id,
                handoff_input_id, started_at_ms
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
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
        .execute(&self.pool)
        .await
        .context("insert orbit_role_executions")?;

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
        sqlx::query(
            r#"
            UPDATE orbit_role_executions
            SET status = $1, resolved_target = $2
            WHERE id = $3
            "#,
        )
        .bind(RoleExecutionStatus::Running.as_str())
        .bind(val)
        .bind(re_id)
        .execute(&self.pool)
        .await
        .context("update orbit_role_executions resolved")?;
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

        sqlx::query(
            r#"
            UPDATE orbit_role_executions
            SET status = $1,
                output_workspace_state_id = $2,
                handoff_output_id = $3,
                finished_at_ms = $4,
                termination_reason = 'success'
            WHERE id = $5
            "#,
        )
        .bind(RoleExecutionStatus::Succeeded.as_str())
        .bind(output_workspace_state_id)
        .bind(handoff_output_id)
        .bind(now_ms)
        .bind(re_id)
        .execute(&self.pool)
        .await
        .context("complete orbit_role_executions success")?;

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

        sqlx::query(
            r#"
            UPDATE orbit_role_executions
            SET status = $1,
                finished_at_ms = $2,
                termination_reason = $3,
                failure_message = $4
            WHERE id = $5
            "#,
        )
        .bind(RoleExecutionStatus::Failed.as_str())
        .bind(now_ms)
        .bind(reason)
        .bind(message)
        .bind(re_id)
        .execute(&self.pool)
        .await
        .context("complete orbit_role_executions failed")?;

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
        sqlx::query(
            r#"
            INSERT INTO orbit_handoff_artifacts (
                id, workflow_run_id, role_execution_id, handoff_type,
                version, workspace_state_id, structured_payload
            ) VALUES ($1, $2, $3, $4, $5, $6, $7)
            "#,
        )
        .bind(&hid)
        .bind(workflow_run_id)
        .bind(role_execution_id)
        .bind(handoff_type.as_str())
        .bind(1)
        .bind(workspace_state_id)
        .bind(&structured_payload)
        .execute(&self.pool)
        .await
        .context("insert orbit_handoff_artifacts")?;

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

        // 3. Phase B6 Invariant: Completion requires FULL regression tier
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

impl RoleRuntimeResolver {
    /// Resolves execution target based on role preferences and simulated or live provider availability.
    pub async fn resolve_target_live(
        pool: &sqlx::PgPool,
        role: &RoleDefinition,
        simulate_quota_exhausted_for: Option<&str>,
    ) -> Result<ResolvedExecutionTarget> {
        let cred_store = crate::credential_registry::CredentialStore::new(pool);
        let credentials = cred_store.list().await?;

        for pref in &role.runtime_preferences {
            if simulate_quota_exhausted_for == Some(pref.as_str()) {
                continue;
            }

            if pref.contains("codex") {
                let avail_store = crate::availability::AvailabilityStore::new(pool);
                for cred in credentials.iter().filter(|c| {
                    c.provider == "codex"
                        && c.status == crate::credential_registry::CredentialStatus::Enrolled
                }) {
                    let is_exhausted = matches!(
                        avail_store.current_for_credential(&cred.identity()).await,
                        Ok(Some(avail))
                            if matches!(
                                avail.state,
                                crate::availability::AvailabilityState::QuotaExhausted
                                    | crate::availability::AvailabilityState::RateLimited
                                    | crate::availability::AvailabilityState::Cooldown
                                    | crate::availability::AvailabilityState::RuntimeUnavailable
                            )
                    );
                    if is_exhausted {
                        continue;
                    }

                    return Ok(ResolvedExecutionTarget {
                        provider: "codex".into(),
                        runtime_interface: "codex-acp".into(),
                        credential_id: Some(cred.reference.clone()),
                        credential_generation: Some(cred.generation as u32),
                        requested_model: Some("gpt-6-luna".into()),
                        resolved_model: Some("gpt-6-luna".into()),
                        runtime_image_digest: Some(
                            crate::codex_credential_enrollment::CODEX_IMAGE_DIGEST.into(),
                        ),
                        resolution_reason: format!(
                            "enrolled ready credential {} matching preference {}",
                            cred.reference, pref
                        ),
                    });
                }
            } else if pref.contains("antigravity") {
                let avail_store = crate::availability::AvailabilityStore::new(pool);
                for cred in credentials.iter().filter(|c| {
                    c.provider == "antigravity"
                        && c.status == crate::credential_registry::CredentialStatus::Enrolled
                }) {
                    let is_exhausted = matches!(
                        avail_store.current_for_credential(&cred.identity()).await,
                        Ok(Some(avail))
                            if matches!(
                                avail.state,
                                crate::availability::AvailabilityState::QuotaExhausted
                                    | crate::availability::AvailabilityState::RateLimited
                                    | crate::availability::AvailabilityState::Cooldown
                                    | crate::availability::AvailabilityState::RuntimeUnavailable
                            )
                    );
                    if is_exhausted {
                        continue;
                    }

                    return Ok(ResolvedExecutionTarget {
                        provider: "antigravity".into(),
                        runtime_interface: "antigravity-acp".into(),
                        credential_id: Some(cred.reference.clone()),
                        credential_generation: Some(cred.generation as u32),
                        requested_model: Some("gemini-3.8-flash".into()),
                        resolved_model: Some("gemini-3.8-flash".into()),
                        runtime_image_digest: Some(
                            crate::credential_enrollment::ANTIGRAVITY_DIGEST.into(),
                        ),
                        resolution_reason: format!(
                            "enrolled ready credential {} matching preference {}",
                            cred.reference, pref
                        ),
                    });
                }
            }
        }

        bail!(
            "failed to resolve live execution target for role {}: all preferences exhausted or no eligible credentials enrolled in CredentialStore",
            role.role_id
        )
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

        let d1 = planner.digest();
        let d2 = implementer.digest();
        let d3 = reviewer.digest();
        assert_ne!(d1, d2);
        assert_ne!(d2, d3);
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
