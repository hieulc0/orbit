//! Production Workflow Coordinator (Phase B3.1).
//! Orchestrates autonomous multi-stage software change workflows
//! using durable state, credential resolution, ACP agent execution,
//! and multi-tier verification.

use crate::{
    acp_contract::{
        Accounting, Auth, AuthMode, Descriptor, FilesystemPolicy, Limits as AcpLimits, ModelPolicy,
        SecurityProfile, TerminalPolicy,
    },
    acp_runtime::{Adapter, AgentNetwork, AuthStore, Launch, Runtime},
    acp_wire::Wire,
    agent::{Binding, Budget},
    codex_credential_enrollment::registered_auth_diagnostic,
    credential_enrollment::{ACP_EXECUTABLE, ANTIGRAVITY_IMAGE, decode_bundle},
    credential_registry::CredentialStore,
    model::*,
    regression_strategy::{
        RegressionPolicy, RegressionStore, SelectionPolicy, VerificationTier, select_verification,
    },
    secret_backend::{LocalPrivateSecretBackend, SecretBackend},
    verification::{
        EnvironmentIdentity, VerificationPolicy, VerificationRun, VerificationRunResult,
        VerificationStore, WorkspaceState,
    },
    workflow::*,
};
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::process::Stdio;
use std::time::Duration;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

const ERR_CLI_WORKFLOW_TERMINAL_DISABLED: &str =
    "CLI_WORKFLOW_TERMINAL_DISABLED: no qualified confined terminal owner is available";

#[derive(Debug)]
struct UnconfirmedRoleCleanup(String);

impl std::fmt::Display for UnconfirmedRoleCleanup {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "ROLE_CLEANUP_UNCONFIRMED: {}", self.0)
    }
}

impl std::error::Error for UnconfirmedRoleCleanup {}

/// Result of a single coordinator execution step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkflowStepResult {
    Advanced {
        from: WorkflowStage,
        to: WorkflowStage,
    },
    Terminal(WorkflowStage),
    Waiting,
}

struct ResolvedWorkflowPolicies {
    verification: Option<VerificationPolicy>,
    regression: Option<RegressionPolicy>,
    selection: Option<SelectionPolicy>,
}

fn workflow_repo_path(wf: &WorkflowRun) -> Result<&Path> {
    let stored = wf
        .repository_path
        .as_deref()
        .context("REPOSITORY_IDENTITY_REQUIRED: workflow has no repository pinned at creation")?;
    let path = Path::new(stored);
    ensure!(
        path.is_absolute(),
        "REPOSITORY_IDENTITY_REQUIRED: stored repository path is relative"
    );
    ensure!(
        path.canonicalize()
            .context("resolve stored workflow repository")?
            == path,
        "REPOSITORY_IDENTITY_MISMATCH: stored repository path no longer resolves to its canonical identity"
    );
    Ok(path)
}

async fn require_candidate_state(
    wf: &WorkflowRun,
    expected_state_id: &str,
) -> Result<WorkspaceState> {
    let state = compute_workspace_state(
        workflow_repo_path(wf)?,
        wf.base_revision.as_deref().unwrap_or("HEAD"),
    )
    .await?;
    ensure!(
        state.state_id == expected_state_id,
        "WORKSPACE_MUTATION_VIOLATION: candidate state '{}' differs from recorded state '{}'",
        state.state_id,
        expected_state_id
    );
    Ok(state)
}

/// Outcome of a role agent execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleExecutionOutcome {
    pub raw_output: String,
    pub agent_execution_ids: Vec<String>,
    pub termination_reason: Option<String>,
}

/// Abstract executor for running role agents.
#[async_trait::async_trait]
pub trait RoleAgentExecutor: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    async fn execute_role(
        &self,
        pool: &PgPool,
        wf_run: &WorkflowRun,
        role_exec: &RoleExecution,
        role: &RoleDefinition,
        target: &ResolvedExecutionTarget,
        task_text: &str,
        repo_path: &Path,
        input_handoff: Option<&HandoffArtifact>,
        cancellation: tokio::sync::watch::Receiver<bool>,
    ) -> Result<RoleExecutionOutcome>;
}

/// Production Workflow Coordinator driving workflow runs to completion.
pub struct WorkflowCoordinator {
    pool: PgPool,
    store: WorkflowStore,
    verification_store: VerificationStore,
    regression_store: RegressionStore,
    executor: Arc<dyn RoleAgentExecutor>,
    active_cancellations: Arc<Mutex<BTreeMap<String, tokio::sync::watch::Sender<bool>>>>,
    quota_selection_policy: RuntimeQuotaSelectionPolicy,
    verification_environment: Option<EnvironmentIdentity>,
}

impl WorkflowCoordinator {
    pub fn new(pool: PgPool, executor: Arc<dyn RoleAgentExecutor>) -> Self {
        let store = WorkflowStore::new(pool.clone());
        let verification_store = VerificationStore::new(pool.clone());
        let regression_store = RegressionStore::new(pool.clone());
        Self {
            pool,
            store,
            verification_store,
            regression_store,
            executor,
            active_cancellations: Arc::new(Mutex::new(BTreeMap::new())),
            quota_selection_policy: RuntimeQuotaSelectionPolicy::default(),
            verification_environment: None,
        }
    }

    pub fn with_quota_selection_policy(
        mut self,
        policy: RuntimeQuotaSelectionPolicy,
    ) -> Result<Self> {
        policy.validate()?;
        self.quota_selection_policy = policy;
        Ok(self)
    }

    /// Pin the isolated execution environment used by workflow verification.
    ///
    /// Without an explicitly supplied profile, verification remains fail-closed.
    pub fn with_verification_environment(
        mut self,
        environment: EnvironmentIdentity,
    ) -> Result<Self> {
        crate::verification::validate_pinned_verification_profile(&environment)?;
        self.verification_environment = Some(environment);
        Ok(self)
    }

    pub fn store(&self) -> &WorkflowStore {
        &self.store
    }

    pub fn verification_store(&self) -> &VerificationStore {
        &self.verification_store
    }

    pub fn regression_store(&self) -> &RegressionStore {
        &self.regression_store
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Advance the workflow run by one state transition.
    pub async fn step(&self, wf_id: &str) -> Result<WorkflowStepResult> {
        let mut claim = self.store.claim_workflow_step(wf_id).await?;
        if claim.is_none() && self.store.recover_orphaned_workflow_step(wf_id).await? {
            claim = self.store.claim_workflow_step(wf_id).await?;
        }
        let Some(claim) = claim else {
            let workflow = self
                .store
                .get_workflow_run(wf_id)
                .await?
                .context("workflow run not found")?;
            return if workflow.status.is_terminal() {
                Ok(WorkflowStepResult::Terminal(workflow.status))
            } else {
                Ok(WorkflowStepResult::Waiting)
            };
        };
        let owned = Self {
            pool: self.pool.clone(),
            store: self.store.with_step_claim(claim.clone()),
            verification_store: VerificationStore::new(self.pool.clone()),
            regression_store: RegressionStore::new(self.pool.clone()),
            executor: Arc::clone(&self.executor),
            active_cancellations: Arc::clone(&self.active_cancellations),
            quota_selection_policy: self.quota_selection_policy,
            verification_environment: self.verification_environment.clone(),
        };
        let (cancel_tx, _cancel_rx) = tokio::sync::watch::channel(false);
        self.active_cancellations
            .lock()
            .unwrap()
            .insert(wf_id.to_owned(), cancel_tx.clone());
        let watch_pool = self.pool.clone();
        let watch_claim = claim.clone();
        let watchdog = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(200));
            loop {
                interval.tick().await;
                let authority: std::result::Result<Option<(String, Option<String>, i64)>, sqlx::Error> =
                    sqlx::query_as(
                        "SELECT status, step_owner_id, step_generation FROM orbit_workflow_runs WHERE id = $1",
                    )
                    .bind(&watch_claim.workflow_run_id)
                    .fetch_optional(&watch_pool)
                    .await;
                let still_owned = matches!(authority,
                    Ok(Some((status, Some(owner), generation)))
                        if status != "CANCELLED" && owner == watch_claim.owner_id && generation == watch_claim.generation);
                if !still_owned {
                    let _ = cancel_tx.send(true);
                    break;
                }
            }
        });
        let result = owned.step_claimed(wf_id).await;
        watchdog.abort();
        self.active_cancellations.lock().unwrap().remove(wf_id);
        let release = self.store.release_workflow_step(&claim).await;
        match (result, release) {
            (Ok(step), Ok(())) => Ok(step),
            (Err(error), Ok(())) => Err(error),
            (_, Err(error)) => {
                let workflow = self.store.get_workflow_run(wf_id).await?;
                if let Some(workflow) = workflow.filter(|workflow| workflow.status.is_terminal()) {
                    Ok(WorkflowStepResult::Terminal(workflow.status))
                } else {
                    Err(error)
                }
            }
        }
    }

    async fn step_claimed(&self, wf_id: &str) -> Result<WorkflowStepResult> {
        let wf = self
            .store
            .get_workflow_run(wf_id)
            .await?
            .context("workflow run not found")?;

        if wf.status.is_terminal() {
            return Ok(WorkflowStepResult::Terminal(wf.status));
        }

        if let Some(resumed) = self.resume_completed_role_step(&wf).await? {
            return Ok(resumed);
        }
        if self
            .store
            .list_role_executions(wf_id)
            .await?
            .iter()
            .any(|role| {
                role.stage == wf.status.as_str()
                    && matches!(
                        role.status,
                        RoleExecutionStatus::Resolving | RoleExecutionStatus::Running
                    )
            })
        {
            bail!(
                "WORKFLOW_ROLE_RECOVERY_REQUIRED: previous role execution has an unconfirmed outcome"
            );
        }

        match wf.status {
            WorkflowStage::Created => {
                self.store
                    .transition_workflow_stage(wf_id, WorkflowStage::Planning, None, None, None)
                    .await?;
                Ok(WorkflowStepResult::Advanced {
                    from: WorkflowStage::Created,
                    to: WorkflowStage::Planning,
                })
            }

            WorkflowStage::Planning => {
                let role = RoleDefinition::planner_v1();
                let target = RoleRuntimeResolver::resolve_target_live_with_policy(
                    &self.pool,
                    &role,
                    None,
                    self.quota_selection_policy,
                )
                .await?;

                let role_exec = self
                    .store
                    .create_role_execution(
                        wf_id,
                        &role,
                        "PLANNING",
                        wf.iteration,
                        wf.current_workspace_state_id.as_deref(),
                        None,
                    )
                    .await?;

                self.store
                    .set_role_execution_resolved(&role_exec.id, &target)
                    .await?;

                let outcome = self
                    .executor
                    .execute_role(
                        &self.pool,
                        &wf,
                        &role_exec,
                        &role,
                        &target,
                        wf.task_prompt.as_deref().unwrap_or(""),
                        workflow_repo_path(&wf)?,
                        None,
                        self.cancellation_receiver(wf_id)?,
                    )
                    .await;

                let outcome = match outcome {
                    Ok(o) => o,
                    Err(e) => {
                        let err_msg = format!("planning role execution failed: {e:#}");
                        self.store
                            .complete_role_execution_failed(
                                &role_exec.id,
                                "ROLE_EXECUTION_FAILED",
                                &err_msg,
                            )
                            .await?;
                        self.store
                            .transition_workflow_stage(
                                wf_id,
                                WorkflowStage::Failed,
                                None,
                                None,
                                Some(&err_msg),
                            )
                            .await?;
                        return Ok(WorkflowStepResult::Terminal(WorkflowStage::Failed));
                    }
                };

                let plan: PlanHandoff = match extract_structured_envelope::<PlanHandoff>(
                    &outcome.raw_output,
                    "PlanHandoff",
                ) {
                    Ok(p) => {
                        if let Err(e) = p.validate() {
                            let err_msg =
                                format!("ROLE_OUTPUT_INVALID: plan validation error: {e:#}");
                            self.store
                                .complete_role_execution_failed(
                                    &role_exec.id,
                                    "ROLE_OUTPUT_INVALID",
                                    &err_msg,
                                )
                                .await?;
                            self.store
                                .transition_workflow_stage(
                                    wf_id,
                                    WorkflowStage::Failed,
                                    None,
                                    None,
                                    Some(&err_msg),
                                )
                                .await?;
                            return Ok(WorkflowStepResult::Terminal(WorkflowStage::Failed));
                        }
                        p
                    }
                    Err(e) => {
                        let err_msg = format!("ROLE_OUTPUT_INVALID: {e:#}");
                        self.store
                            .complete_role_execution_failed(
                                &role_exec.id,
                                "ROLE_OUTPUT_INVALID",
                                &err_msg,
                            )
                            .await?;
                        self.store
                            .transition_workflow_stage(
                                wf_id,
                                WorkflowStage::Failed,
                                None,
                                None,
                                Some(&err_msg),
                            )
                            .await?;
                        return Ok(WorkflowStepResult::Terminal(WorkflowStage::Failed));
                    }
                };

                let handoff = self
                    .store
                    .save_handoff_artifact(
                        wf_id,
                        Some(&role_exec.id),
                        HandoffType::Plan,
                        wf.current_workspace_state_id.as_deref(),
                        serde_json::to_value(&plan)?,
                    )
                    .await?;

                self.store
                    .complete_role_execution_success(
                        &role_exec.id,
                        wf.current_workspace_state_id.as_deref(),
                        Some(&handoff.id),
                    )
                    .await?;

                self.store
                    .transition_workflow_stage(
                        wf_id,
                        WorkflowStage::Implementing,
                        wf.current_workspace_state_id.as_deref(),
                        None,
                        None,
                    )
                    .await?;

                Ok(WorkflowStepResult::Advanced {
                    from: WorkflowStage::Planning,
                    to: WorkflowStage::Implementing,
                })
            }

            WorkflowStage::Implementing => {
                let role = RoleDefinition::implementer_v1();
                let target = RoleRuntimeResolver::resolve_target_live_with_policy(
                    &self.pool,
                    &role,
                    None,
                    self.quota_selection_policy,
                )
                .await?;

                let plan_handoff = self
                    .store
                    .get_latest_handoff_of_type(wf_id, HandoffType::Plan)
                    .await?;

                let role_exec = self
                    .store
                    .create_role_execution(
                        wf_id,
                        &role,
                        "IMPLEMENTING",
                        wf.iteration,
                        wf.current_workspace_state_id.as_deref(),
                        plan_handoff.as_ref().map(|h| h.id.as_str()),
                    )
                    .await?;

                self.store
                    .set_role_execution_resolved(&role_exec.id, &target)
                    .await?;

                let lock_holder_id = role_exec.id.clone();
                self.store
                    .acquire_workspace_mutation_lock(&wf.attempt_id, &lock_holder_id)
                    .await?;

                let outcome = self
                    .executor
                    .execute_role(
                        &self.pool,
                        &wf,
                        &role_exec,
                        &role,
                        &target,
                        wf.task_prompt.as_deref().unwrap_or(""),
                        workflow_repo_path(&wf)?,
                        plan_handoff.as_ref(),
                        self.cancellation_receiver(wf_id)?,
                    )
                    .await;

                let outcome = match outcome {
                    Ok(o) => o,
                    Err(e) => {
                        if e.downcast_ref::<UnconfirmedRoleCleanup>().is_none() {
                            let _ = self
                                .store
                                .release_workspace_mutation_lock(&wf.attempt_id, &lock_holder_id)
                                .await;
                        }
                        let err_msg = format!("implementer role execution failed: {e:#}");
                        self.store
                            .complete_role_execution_failed(
                                &role_exec.id,
                                "ROLE_EXECUTION_FAILED",
                                &err_msg,
                            )
                            .await?;
                        self.store
                            .transition_workflow_stage(
                                wf_id,
                                WorkflowStage::Failed,
                                None,
                                None,
                                Some(&err_msg),
                            )
                            .await?;
                        return Ok(WorkflowStepResult::Terminal(WorkflowStage::Failed));
                    }
                };

                let impl_handoff: ImplementationHandoff = match extract_structured_envelope::<
                    ImplementationHandoff,
                >(
                    &outcome.raw_output,
                    "ImplementationHandoff",
                ) {
                    Ok(h) => {
                        if let Err(e) = h.validate() {
                            let _ = self
                                .store
                                .release_workspace_mutation_lock(&wf.attempt_id, &lock_holder_id)
                                .await;
                            let err_msg = format!(
                                "ROLE_OUTPUT_INVALID: implementation validation error: {e:#}"
                            );
                            self.store
                                .complete_role_execution_failed(
                                    &role_exec.id,
                                    "ROLE_OUTPUT_INVALID",
                                    &err_msg,
                                )
                                .await?;
                            self.store
                                .transition_workflow_stage(
                                    wf_id,
                                    WorkflowStage::Failed,
                                    None,
                                    None,
                                    Some(&err_msg),
                                )
                                .await?;
                            return Ok(WorkflowStepResult::Terminal(WorkflowStage::Failed));
                        }
                        h
                    }
                    Err(e) => {
                        let _ = self
                            .store
                            .release_workspace_mutation_lock(&wf.attempt_id, &lock_holder_id)
                            .await;
                        let err_msg = format!("ROLE_OUTPUT_INVALID: {e:#}");
                        self.store
                            .complete_role_execution_failed(
                                &role_exec.id,
                                "ROLE_OUTPUT_INVALID",
                                &err_msg,
                            )
                            .await?;
                        self.store
                            .transition_workflow_stage(
                                wf_id,
                                WorkflowStage::Failed,
                                None,
                                None,
                                Some(&err_msg),
                            )
                            .await?;
                        return Ok(WorkflowStepResult::Terminal(WorkflowStage::Failed));
                    }
                };

                let repo_path = workflow_repo_path(&wf)?;
                let baseline = wf.base_revision.as_deref().unwrap_or("HEAD");
                let new_ws_state = compute_workspace_state(repo_path, baseline).await?;

                let handoff = self
                    .store
                    .save_handoff_artifact(
                        wf_id,
                        Some(&role_exec.id),
                        HandoffType::Implementation,
                        Some(&new_ws_state.state_id),
                        serde_json::to_value(&impl_handoff)?,
                    )
                    .await?;

                self.store
                    .complete_role_execution_success(
                        &role_exec.id,
                        Some(&new_ws_state.state_id),
                        Some(&handoff.id),
                    )
                    .await?;

                self.store
                    .release_workspace_mutation_lock(&wf.attempt_id, &lock_holder_id)
                    .await?;

                self.store
                    .transition_workflow_stage(
                        wf_id,
                        WorkflowStage::Verifying,
                        Some(&new_ws_state.state_id),
                        None,
                        None,
                    )
                    .await?;

                Ok(WorkflowStepResult::Advanced {
                    from: WorkflowStage::Implementing,
                    to: WorkflowStage::Verifying,
                })
            }

            WorkflowStage::Verifying => {
                let ws_state_id = wf
                    .current_workspace_state_id
                    .as_deref()
                    .context("missing workspace state in verifying stage")?;

                let ws_state = require_candidate_state(&wf, ws_state_id).await?;

                let policies = self.resolve_workflow_policies(&wf).await?;

                // Run FAST tier verification first
                let fast_run = self
                    .run_tier_verification(
                        &wf,
                        &ws_state,
                        VerificationTier::Fast,
                        policies.verification.as_ref(),
                        policies.regression.as_ref(),
                        policies.selection.as_ref(),
                    )
                    .await?;
                require_candidate_state(&wf, ws_state_id).await?;

                if fast_run.overall_result != Some(VerificationRunResult::Passed) {
                    let fail_reason = "Fast tier verification failed".to_string();
                    self.record_failure_evidence(
                        wf_id,
                        "VERIFYING_FAST",
                        Some(&fast_run.id),
                        &fail_reason,
                    )
                    .await?;
                    self.store
                        .transition_workflow_stage(
                            wf_id,
                            WorkflowStage::Repairing,
                            Some(ws_state_id),
                            None,
                            Some(&fail_reason),
                        )
                        .await?;
                    return Ok(WorkflowStepResult::Advanced {
                        from: WorkflowStage::Verifying,
                        to: WorkflowStage::Repairing,
                    });
                }

                // Run STANDARD tier verification
                let std_run = self
                    .run_tier_verification(
                        &wf,
                        &ws_state,
                        VerificationTier::Standard,
                        policies.verification.as_ref(),
                        policies.regression.as_ref(),
                        policies.selection.as_ref(),
                    )
                    .await?;
                require_candidate_state(&wf, ws_state_id).await?;

                if std_run.overall_result != Some(VerificationRunResult::Passed) {
                    let fail_reason = "Standard tier verification failed".to_string();
                    self.record_failure_evidence(
                        wf_id,
                        "VERIFYING_STANDARD",
                        Some(&std_run.id),
                        &fail_reason,
                    )
                    .await?;
                    self.store
                        .transition_workflow_stage(
                            wf_id,
                            WorkflowStage::Repairing,
                            Some(ws_state_id),
                            None,
                            Some(&fail_reason),
                        )
                        .await?;
                    return Ok(WorkflowStepResult::Advanced {
                        from: WorkflowStage::Verifying,
                        to: WorkflowStage::Repairing,
                    });
                }

                // Both FAST and STANDARD passed -> Advance to REVIEWING
                self.store
                    .transition_workflow_stage(
                        wf_id,
                        WorkflowStage::Reviewing,
                        Some(ws_state_id),
                        None,
                        None,
                    )
                    .await?;

                Ok(WorkflowStepResult::Advanced {
                    from: WorkflowStage::Verifying,
                    to: WorkflowStage::Reviewing,
                })
            }

            WorkflowStage::Repairing => {
                if wf.iteration >= wf.max_iterations {
                    self.store
                        .transition_workflow_stage(
                            wf_id,
                            WorkflowStage::Exhausted,
                            wf.current_workspace_state_id.as_deref(),
                            None,
                            Some("maximum repair iterations reached without qualification"),
                        )
                        .await?;
                    return Ok(WorkflowStepResult::Terminal(WorkflowStage::Exhausted));
                }

                let next_iteration = wf.iteration + 1;
                self.store
                    .advance_repair_iteration(wf_id, wf.iteration, next_iteration)
                    .await?;

                let role = RoleDefinition::implementer_v1();
                let target = RoleRuntimeResolver::resolve_target_live_with_policy(
                    &self.pool,
                    &role,
                    None,
                    self.quota_selection_policy,
                )
                .await?;

                let failure_handoff = self
                    .store
                    .get_latest_handoff_of_type(wf_id, HandoffType::FailureEvidence)
                    .await?;

                let role_exec = self
                    .store
                    .create_role_execution(
                        wf_id,
                        &role,
                        "REPAIRING",
                        next_iteration,
                        wf.current_workspace_state_id.as_deref(),
                        failure_handoff.as_ref().map(|h| h.id.as_str()),
                    )
                    .await?;

                self.store
                    .set_role_execution_resolved(&role_exec.id, &target)
                    .await?;

                let lock_holder_id = role_exec.id.clone();
                self.store
                    .acquire_workspace_mutation_lock(&wf.attempt_id, &lock_holder_id)
                    .await?;

                let outcome = self
                    .executor
                    .execute_role(
                        &self.pool,
                        &wf,
                        &role_exec,
                        &role,
                        &target,
                        wf.task_prompt.as_deref().unwrap_or(""),
                        workflow_repo_path(&wf)?,
                        failure_handoff.as_ref(),
                        self.cancellation_receiver(wf_id)?,
                    )
                    .await;

                let outcome = match outcome {
                    Ok(o) => o,
                    Err(e) => {
                        if e.downcast_ref::<UnconfirmedRoleCleanup>().is_none() {
                            let _ = self
                                .store
                                .release_workspace_mutation_lock(&wf.attempt_id, &lock_holder_id)
                                .await;
                        }
                        let err_msg = format!("repair role execution failed: {e:#}");
                        self.store
                            .complete_role_execution_failed(
                                &role_exec.id,
                                "ROLE_EXECUTION_FAILED",
                                &err_msg,
                            )
                            .await?;
                        self.store
                            .transition_workflow_stage(
                                wf_id,
                                WorkflowStage::Failed,
                                None,
                                None,
                                Some(&err_msg),
                            )
                            .await?;
                        return Ok(WorkflowStepResult::Terminal(WorkflowStage::Failed));
                    }
                };

                let impl_handoff: ImplementationHandoff = match extract_structured_envelope::<
                    ImplementationHandoff,
                >(
                    &outcome.raw_output,
                    "ImplementationHandoff",
                ) {
                    Ok(h) => {
                        if let Err(e) = h.validate() {
                            let _ = self
                                .store
                                .release_workspace_mutation_lock(&wf.attempt_id, &lock_holder_id)
                                .await;
                            let err_msg = format!(
                                "ROLE_OUTPUT_INVALID: repair implementation invalid: {e:#}"
                            );
                            self.store
                                .complete_role_execution_failed(
                                    &role_exec.id,
                                    "ROLE_OUTPUT_INVALID",
                                    &err_msg,
                                )
                                .await?;
                            self.store
                                .transition_workflow_stage(
                                    wf_id,
                                    WorkflowStage::Failed,
                                    None,
                                    None,
                                    Some(&err_msg),
                                )
                                .await?;
                            return Ok(WorkflowStepResult::Terminal(WorkflowStage::Failed));
                        }
                        h
                    }
                    Err(e) => {
                        let _ = self
                            .store
                            .release_workspace_mutation_lock(&wf.attempt_id, &lock_holder_id)
                            .await;
                        let err_msg = format!("ROLE_OUTPUT_INVALID: {e:#}");
                        self.store
                            .complete_role_execution_failed(
                                &role_exec.id,
                                "ROLE_OUTPUT_INVALID",
                                &err_msg,
                            )
                            .await?;
                        self.store
                            .transition_workflow_stage(
                                wf_id,
                                WorkflowStage::Failed,
                                None,
                                None,
                                Some(&err_msg),
                            )
                            .await?;
                        return Ok(WorkflowStepResult::Terminal(WorkflowStage::Failed));
                    }
                };

                let repo_path = workflow_repo_path(&wf)?;
                let baseline = wf.base_revision.as_deref().unwrap_or("HEAD");
                let new_ws_state = compute_workspace_state(repo_path, baseline).await?;

                let handoff = self
                    .store
                    .save_handoff_artifact(
                        wf_id,
                        Some(&role_exec.id),
                        HandoffType::Implementation,
                        Some(&new_ws_state.state_id),
                        serde_json::to_value(&impl_handoff)?,
                    )
                    .await?;

                self.store
                    .complete_role_execution_success(
                        &role_exec.id,
                        Some(&new_ws_state.state_id),
                        Some(&handoff.id),
                    )
                    .await?;

                self.store
                    .release_workspace_mutation_lock(&wf.attempt_id, &lock_holder_id)
                    .await?;

                self.store
                    .transition_workflow_stage(
                        wf_id,
                        WorkflowStage::Verifying,
                        Some(&new_ws_state.state_id),
                        None,
                        None,
                    )
                    .await?;

                Ok(WorkflowStepResult::Advanced {
                    from: WorkflowStage::Repairing,
                    to: WorkflowStage::Verifying,
                })
            }

            WorkflowStage::Reviewing => {
                let expected_state_id = wf
                    .current_workspace_state_id
                    .as_deref()
                    .context("missing workspace state in reviewing stage")?;
                require_candidate_state(&wf, expected_state_id).await?;
                if let Err(error) = review_candidate_diff(
                    workflow_repo_path(&wf)?,
                    wf.base_revision.as_deref().unwrap_or("HEAD"),
                )
                .await
                {
                    let reason =
                        format!("REVIEW_ERROR: generate reviewer candidate diff: {error:#}");
                    self.store
                        .transition_workflow_stage(
                            wf_id,
                            WorkflowStage::Failed,
                            None,
                            None,
                            Some(&reason),
                        )
                        .await?;
                    return Ok(WorkflowStepResult::Terminal(WorkflowStage::Failed));
                }
                let role = RoleDefinition::reviewer_v1();
                let target = RoleRuntimeResolver::resolve_target_live_with_policy(
                    &self.pool,
                    &role,
                    None,
                    self.quota_selection_policy,
                )
                .await?;

                let impl_handoff = self
                    .store
                    .get_latest_handoff_of_type(wf_id, HandoffType::Implementation)
                    .await?;

                let role_exec = self
                    .store
                    .create_role_execution(
                        wf_id,
                        &role,
                        "REVIEWING",
                        wf.iteration,
                        wf.current_workspace_state_id.as_deref(),
                        impl_handoff.as_ref().map(|h| h.id.as_str()),
                    )
                    .await?;

                self.store
                    .set_role_execution_resolved(&role_exec.id, &target)
                    .await?;

                let outcome = self
                    .executor
                    .execute_role(
                        &self.pool,
                        &wf,
                        &role_exec,
                        &role,
                        &target,
                        wf.task_prompt.as_deref().unwrap_or(""),
                        workflow_repo_path(&wf)?,
                        impl_handoff.as_ref(),
                        self.cancellation_receiver(wf_id)?,
                    )
                    .await;

                let outcome = match outcome {
                    Ok(o) => o,
                    Err(e) => {
                        let err_msg = format!("reviewer role execution failed: {e:#}");
                        self.store
                            .complete_role_execution_failed(
                                &role_exec.id,
                                "ROLE_EXECUTION_FAILED",
                                &err_msg,
                            )
                            .await?;
                        self.store
                            .transition_workflow_stage(
                                wf_id,
                                WorkflowStage::Failed,
                                None,
                                None,
                                Some(&err_msg),
                            )
                            .await?;
                        return Ok(WorkflowStepResult::Terminal(WorkflowStage::Failed));
                    }
                };

                require_candidate_state(&wf, expected_state_id).await?;

                let review_dec: ReviewDecision = match extract_structured_envelope::<ReviewDecision>(
                    &outcome.raw_output,
                    "ReviewDecision",
                ) {
                    Ok(d) => {
                        if let Err(e) = d.validate() {
                            let err_msg =
                                format!("ROLE_OUTPUT_INVALID: review validation error: {e:#}");
                            self.store
                                .complete_role_execution_failed(
                                    &role_exec.id,
                                    "ROLE_OUTPUT_INVALID",
                                    &err_msg,
                                )
                                .await?;
                            self.store
                                .transition_workflow_stage(
                                    wf_id,
                                    WorkflowStage::Failed,
                                    None,
                                    None,
                                    Some(&err_msg),
                                )
                                .await?;
                            return Ok(WorkflowStepResult::Terminal(WorkflowStage::Failed));
                        }
                        d
                    }
                    Err(e) => {
                        let err_msg = format!("ROLE_OUTPUT_INVALID: {e:#}");
                        self.store
                            .complete_role_execution_failed(
                                &role_exec.id,
                                "ROLE_OUTPUT_INVALID",
                                &err_msg,
                            )
                            .await?;
                        self.store
                            .transition_workflow_stage(
                                wf_id,
                                WorkflowStage::Failed,
                                None,
                                None,
                                Some(&err_msg),
                            )
                            .await?;
                        return Ok(WorkflowStepResult::Terminal(WorkflowStage::Failed));
                    }
                };

                let handoff = self
                    .store
                    .save_handoff_artifact(
                        wf_id,
                        Some(&role_exec.id),
                        HandoffType::Review,
                        wf.current_workspace_state_id.as_deref(),
                        serde_json::to_value(&review_dec)?,
                    )
                    .await?;

                self.store
                    .complete_role_execution_success(
                        &role_exec.id,
                        wf.current_workspace_state_id.as_deref(),
                        Some(&handoff.id),
                    )
                    .await?;

                match review_dec.decision {
                    ReviewDecisionStatus::Approve => {
                        self.store
                            .transition_workflow_stage(
                                wf_id,
                                WorkflowStage::Regression,
                                wf.current_workspace_state_id.as_deref(),
                                None,
                                None,
                            )
                            .await?;
                        Ok(WorkflowStepResult::Advanced {
                            from: WorkflowStage::Reviewing,
                            to: WorkflowStage::Regression,
                        })
                    }
                    ReviewDecisionStatus::ChangesRequested => {
                        let reason = format!(
                            "Review requested changes: {}. Findings: {}",
                            review_dec.summary,
                            review_dec.requested_changes.join("; ")
                        );
                        self.record_failure_evidence(wf_id, "REVIEWING", None, &reason)
                            .await?;
                        self.store
                            .transition_workflow_stage(
                                wf_id,
                                WorkflowStage::Repairing,
                                wf.current_workspace_state_id.as_deref(),
                                None,
                                Some(&reason),
                            )
                            .await?;
                        Ok(WorkflowStepResult::Advanced {
                            from: WorkflowStage::Reviewing,
                            to: WorkflowStage::Repairing,
                        })
                    }
                    ReviewDecisionStatus::Blocked => {
                        let reason = format!("Review rejected / blocked: {}", review_dec.summary);
                        self.store
                            .transition_workflow_stage(
                                wf_id,
                                WorkflowStage::Failed,
                                None,
                                None,
                                Some(&reason),
                            )
                            .await?;
                        Ok(WorkflowStepResult::Terminal(WorkflowStage::Failed))
                    }
                }
            }

            WorkflowStage::Regression => {
                let ws_state_id = wf
                    .current_workspace_state_id
                    .as_deref()
                    .context("missing workspace state in regression stage")?;

                let repo_path = workflow_repo_path(&wf)?;
                let baseline = wf.base_revision.as_deref().unwrap_or("HEAD");
                let ws_state = require_candidate_state(&wf, ws_state_id).await?;

                let policies = self.resolve_workflow_policies(&wf).await?;

                // Run FULL tier regression verification
                let full_run = self
                    .run_tier_verification(
                        &wf,
                        &ws_state,
                        VerificationTier::Full,
                        policies.verification.as_ref(),
                        policies.regression.as_ref(),
                        policies.selection.as_ref(),
                    )
                    .await?;

                if full_run.overall_result != Some(VerificationRunResult::Passed) {
                    let fail_reason = "Full regression verification failed".to_string();
                    self.record_failure_evidence(
                        wf_id,
                        "REGRESSION_FULL",
                        Some(&full_run.id),
                        &fail_reason,
                    )
                    .await?;
                    self.store
                        .transition_workflow_stage(
                            wf_id,
                            WorkflowStage::Repairing,
                            Some(ws_state_id),
                            None,
                            Some(&fail_reason),
                        )
                        .await?;
                    return Ok(WorkflowStepResult::Advanced {
                        from: WorkflowStage::Regression,
                        to: WorkflowStage::Repairing,
                    });
                }

                // Workspace Mutation Invariant check (Requirement 27)
                // Immediately before final completion, inspect workspace on disk.
                // Must strictly match the reviewed & qualified ws_state_id!
                let disk_ws_state = compute_workspace_state(repo_path, baseline).await?;
                ensure!(
                    disk_ws_state.state_id == *ws_state_id,
                    "WORKSPACE_MUTATION_VIOLATION: workspace state on disk '{}' does not match verified state '{}'",
                    disk_ws_state.state_id,
                    ws_state_id
                );

                // Assert completion invariant
                self.store.check_completion_invariant(wf_id).await?;

                // Transition to COMPLETED
                self.store
                    .transition_workflow_stage(
                        wf_id,
                        WorkflowStage::Completed,
                        Some(ws_state_id),
                        None,
                        None,
                    )
                    .await?;

                Ok(WorkflowStepResult::Terminal(WorkflowStage::Completed))
            }

            WorkflowStage::Completed
            | WorkflowStage::Failed
            | WorkflowStage::Cancelled
            | WorkflowStage::Exhausted => Ok(WorkflowStepResult::Terminal(wf.status)),
        }
    }

    fn cancellation_receiver(&self, wf_id: &str) -> Result<tokio::sync::watch::Receiver<bool>> {
        self.active_cancellations
            .lock()
            .unwrap()
            .get(wf_id)
            .map(tokio::sync::watch::Sender::subscribe)
            .context("WORKFLOW_CANCELLATION_OWNER_MISSING")
    }

    /// A provider handoff may have been persisted before the stage update.
    /// Reuse that completed role instead of starting another external turn.
    async fn resume_completed_role_step(
        &self,
        wf: &WorkflowRun,
    ) -> Result<Option<WorkflowStepResult>> {
        if !matches!(
            wf.status,
            WorkflowStage::Planning
                | WorkflowStage::Implementing
                | WorkflowStage::Repairing
                | WorkflowStage::Reviewing
        ) {
            return Ok(None);
        }
        let roles = self.store.list_role_executions(&wf.id).await?;
        let Some(role) = roles.iter().rev().find(|role| {
            role.stage == wf.status.as_str()
                && role.iteration == wf.iteration
                && role.status == RoleExecutionStatus::Succeeded
                && role.handoff_output_id.is_some()
        }) else {
            return Ok(None);
        };
        let next = match wf.status {
            WorkflowStage::Planning => WorkflowStage::Implementing,
            WorkflowStage::Implementing | WorkflowStage::Repairing => {
                let state_id = role
                    .output_workspace_state_id
                    .as_deref()
                    .context("completed implementation has no workspace state")?;
                require_candidate_state(wf, state_id).await?;
                if self
                    .store
                    .check_workspace_mutation_lock(&wf.attempt_id, &role.id)
                    .await?
                {
                    self.store
                        .release_workspace_mutation_lock(&wf.attempt_id, &role.id)
                        .await?;
                }
                WorkflowStage::Verifying
            }
            WorkflowStage::Reviewing => {
                let state_id = wf
                    .current_workspace_state_id
                    .as_deref()
                    .context("reviewed workspace state missing")?;
                require_candidate_state(wf, state_id).await?;
                let handoff = self
                    .store
                    .get_handoff_artifact(role.handoff_output_id.as_deref().unwrap())
                    .await?
                    .context("completed reviewer handoff missing")?;
                let decision: ReviewDecision = serde_json::from_value(handoff.structured_payload)?;
                match decision.decision {
                    ReviewDecisionStatus::Approve => WorkflowStage::Regression,
                    ReviewDecisionStatus::ChangesRequested => WorkflowStage::Repairing,
                    ReviewDecisionStatus::Blocked => WorkflowStage::Failed,
                }
            }
            _ => unreachable!(),
        };
        let state_id = role
            .output_workspace_state_id
            .as_deref()
            .or(wf.current_workspace_state_id.as_deref());
        self.store
            .transition_workflow_stage(&wf.id, next, state_id, None, None)
            .await?;
        Ok(Some(if next.is_terminal() {
            WorkflowStepResult::Terminal(next)
        } else {
            WorkflowStepResult::Advanced {
                from: wf.status,
                to: next,
            }
        }))
    }

    /// Autonomous loop stepping the workflow until it reaches a terminal state.
    pub async fn run_to_completion(&self, wf_id: &str) -> Result<WorkflowRun> {
        loop {
            let step_res = self.step(wf_id).await?;
            match step_res {
                WorkflowStepResult::Advanced { .. } => continue,
                WorkflowStepResult::Terminal(_) => {
                    let wf = self
                        .store
                        .get_workflow_run(wf_id)
                        .await?
                        .context("workflow run not found after completion")?;
                    return Ok(wf);
                }
                WorkflowStepResult::Waiting => {
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                }
            }
        }
    }

    /// Cancel atomically and revoke callback authority while external work is accounted for.
    pub async fn cancel_workflow(&self, wf_id: &str, reason: &str) -> Result<WorkflowRun> {
        let now_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as i64;
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query(
            "UPDATE orbit_workflow_runs SET status = 'CANCELLED', current_stage = 'CANCELLED', cancellation_reason = $2, finished_at_ms = $3, step_owner_id = NULL, step_owner_pid = NULL, step_owner_started_at_ms = NULL, step_generation = step_generation + 1 WHERE id = $1 AND status NOT IN ('COMPLETED', 'FAILED', 'CANCELLED', 'EXHAUSTED')",
        )
        .bind(wf_id)
        .bind(reason)
        .bind(now_ms)
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() == 1 {
            sqlx::query(
                "UPDATE orbit_role_executions SET status = 'CANCELLED', termination_reason = $1, finished_at_ms = $2 WHERE workflow_run_id = $3 AND status IN ('PENDING', 'RESOLVING', 'RUNNING')",
            )
            .bind(reason)
            .bind(now_ms)
            .bind(wf_id)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "UPDATE orbit_attempt_workspace_locks locks SET revoked_at_ms = $2 FROM orbit_role_executions re WHERE locks.holder_role_execution_id = re.id AND re.workflow_run_id = $1",
            )
            .bind(wf_id)
            .bind(now_ms)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        if let Some(sender) = self.active_cancellations.lock().unwrap().get(wf_id) {
            let _ = sender.send(true);
        }
        self.store
            .get_workflow_run(wf_id)
            .await?
            .context("workflow run not found after cancel")
    }

    /// Helper to record failure evidence as a handoff artifact for repair stages.
    async fn record_failure_evidence(
        &self,
        wf_id: &str,
        stage: &str,
        verif_run_id: Option<&str>,
        error_msg: &str,
    ) -> Result<()> {
        let mut failed_steps = Vec::new();
        let mut stdout_previews = BTreeMap::new();
        let mut stderr_previews = BTreeMap::new();
        if let Some(run_id) = verif_run_id
            && let Some(run) = self.verification_store.get_run(run_id).await?
        {
            for step in run.step_runs.iter().filter(|step| {
                step.required && step.status != crate::verification::VerificationStepStatus::Passed
            }) {
                failed_steps.push(step.step_id.clone());
                if let Some(stdout) = &step.stdout_preview {
                    stdout_previews.insert(step.step_id.clone(), stdout.clone());
                }
                if let Some(stderr) = &step.stderr_preview {
                    stderr_previews.insert(step.step_id.clone(), stderr.clone());
                }
            }
        }
        let evidence = FailureEvidenceHandoff {
            failed_stage: stage.to_string(),
            verification_run_id: verif_run_id.map(|s| s.to_string()),
            failed_steps,
            error_summary: error_msg.to_string(),
            stdout_previews,
            stderr_previews,
        };

        self.store
            .save_handoff_artifact(
                wf_id,
                None,
                HandoffType::FailureEvidence,
                None,
                serde_json::to_value(&evidence)?,
            )
            .await?;

        Ok(())
    }

    async fn resolve_workflow_policies(
        &self,
        wf: &WorkflowRun,
    ) -> Result<ResolvedWorkflowPolicies> {
        let verification = match (
            wf.verification_policy_id.as_deref(),
            wf.verification_policy_version,
            wf.verification_policy_digest.as_deref(),
        ) {
            (None, None, None) => None,
            (Some(id), Some(version), Some(expected_digest)) => {
                let policy = self
                    .verification_store
                    .get_policy(id, version)
                    .await?
                    .with_context(|| {
                        format!("pinned verification policy '{id}' version {version} is missing")
                    })?;
                ensure!(
                    policy.digest() == expected_digest,
                    "POLICY_DIGEST_MISMATCH: workflow verification policy '{id}' version {version} no longer matches its pinned content"
                );
                Some(policy)
            }
            _ => {
                bail!("INCOMPLETE_POLICY_PIN: workflow verification policy reference is incomplete")
            }
        };

        let regression = match (
            wf.regression_policy_id.as_deref(),
            wf.regression_policy_version,
            wf.regression_policy_digest.as_deref(),
        ) {
            (None, None, None) => None,
            (Some(id), Some(version), Some(expected_digest)) => {
                let policy = self
                    .regression_store
                    .get_regression_policy(id, version)
                    .await?
                    .with_context(|| {
                        format!("pinned regression policy '{id}' version {version} is missing")
                    })?;
                ensure!(
                    policy.digest() == expected_digest,
                    "POLICY_DIGEST_MISMATCH: workflow regression policy '{id}' version {version} no longer matches its pinned content"
                );
                Some(policy)
            }
            _ => bail!("INCOMPLETE_POLICY_PIN: workflow regression policy reference is incomplete"),
        };

        let selection = match (
            wf.selection_policy_id.as_deref(),
            wf.selection_policy_version,
            wf.selection_policy_digest.as_deref(),
        ) {
            (None, None, None) => None,
            (Some(id), Some(version), Some(expected_digest)) => {
                let policy = self
                    .regression_store
                    .get_selection_policy(id, version)
                    .await?
                    .with_context(|| {
                        format!("pinned selection policy '{id}' version {version} is missing")
                    })?;
                ensure!(
                    policy.digest() == expected_digest,
                    "POLICY_DIGEST_MISMATCH: workflow selection policy '{id}' version {version} no longer matches its pinned content"
                );
                Some(policy)
            }
            _ => bail!("INCOMPLETE_POLICY_PIN: workflow selection policy reference is incomplete"),
        };

        if let Some(regression_policy) = &regression {
            match (
                regression_policy.selection_policy_id.as_deref(),
                regression_policy.selection_policy_version,
                regression_policy.selection_policy_digest.as_deref(),
                selection.as_ref(),
            ) {
                (None, None, None, None) | (None, None, None, Some(_)) => {}
                (Some(id), Some(version), Some(digest), Some(selection_policy)) => ensure!(
                    id == selection_policy.id
                        && version == selection_policy.version
                        && digest == selection_policy.digest(),
                    "POLICY_DIGEST_MISMATCH: regression policy selection reference does not match the workflow selection policy"
                ),
                (Some(id), Some(version), Some(_), None) => bail!(
                    "pinned regression policy references missing selection policy '{id}' version {version}"
                ),
                _ => bail!(
                    "INCOMPLETE_POLICY_PIN: regression policy selection reference is incomplete"
                ),
            }
        }

        Ok(ResolvedWorkflowPolicies {
            verification,
            regression,
            selection,
        })
    }

    /// Execute tier verification with selection or default plan.
    async fn run_tier_verification(
        &self,
        wf: &WorkflowRun,
        ws_state: &WorkspaceState,
        tier: VerificationTier,
        policy: Option<&VerificationPolicy>,
        reg_policy: Option<&RegressionPolicy>,
        sel_policy: Option<&SelectionPolicy>,
    ) -> Result<VerificationRun> {
        let repo_path = workflow_repo_path(wf)?;
        let env = self.verification_environment.clone().unwrap_or_default();
        crate::verification::validate_pinned_verification_profile(&env)?;

        if let Some(sp) = sel_policy {
            let changed_files = changed_files_for_selection(
                repo_path,
                wf.base_revision.as_deref().unwrap_or("HEAD"),
            )
            .await?;
            let previous_failed_checks = self.previous_failed_checks(&wf.id).await?;
            let reviewer_escalations = self.reviewer_escalations(&wf.id).await?;
            let selected_plan = select_verification(
                sp,
                reg_policy,
                &ws_state.state_id,
                tier,
                &changed_files,
                &previous_failed_checks,
                &reviewer_escalations,
            )?;
            crate::regression_strategy::execute_selected_verification_plan(
                &self.verification_store,
                &wf.attempt_id,
                ws_state,
                &selected_plan,
                repo_path,
                env,
                reg_policy,
                Some(self.cancellation_receiver(&wf.id)?),
            )
            .await
        } else {
            bail!(
                "VERIFICATION_ACTIONS_UNRESOLVED: workflow tier has no pinned selection policy with declared verification actions (verification policy present: {})",
                policy.is_some()
            );
        }
    }

    async fn previous_failed_checks(&self, wf_id: &str) -> Result<Vec<String>> {
        let payloads: Vec<serde_json::Value> = sqlx::query_scalar(
            "SELECT structured_payload FROM orbit_handoff_artifacts WHERE workflow_run_id = $1 AND handoff_type = 'FAILURE_EVIDENCE' ORDER BY created_at",
        )
        .bind(wf_id)
        .fetch_all(&self.pool)
        .await?;
        let mut checks = std::collections::BTreeSet::new();
        for payload in payloads {
            let evidence: FailureEvidenceHandoff = serde_json::from_value(payload)
                .context("decode persisted verification failure evidence")?;
            checks.extend(evidence.failed_steps);
        }
        Ok(checks.into_iter().collect())
    }

    async fn reviewer_escalations(&self, wf_id: &str) -> Result<Vec<String>> {
        let Some(handoff) = self
            .store
            .get_latest_handoff_of_type(wf_id, HandoffType::Review)
            .await?
        else {
            return Ok(Vec::new());
        };
        let review: ReviewDecision = serde_json::from_value(handoff.structured_payload)
            .context("decode persisted reviewer decision")?;
        Ok(review.suggested_additional_checks)
    }
}

async fn changed_files_for_selection(repo_path: &Path, baseline: &str) -> Result<Vec<String>> {
    let mut changed = std::collections::BTreeSet::new();
    for args in [
        vec!["diff", "--name-only", "-z", "--no-renames", baseline, "--"],
        vec!["ls-files", "--others", "--exclude-standard", "-z"],
    ] {
        let output = crate::tool_surface::safe_git_command(repo_path, &args)
            .output()
            .await
            .context("start git while collecting verification selection inputs")?;
        ensure!(
            output.status.success(),
            "GIT_SELECTION_INPUT_FAILED: git returned {} while collecting changed paths: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let stdout = String::from_utf8(output.stdout)
            .context("GIT_SELECTION_INPUT_FAILED: changed path output is not UTF-8")?;
        changed.extend(
            stdout
                .split('\0')
                .filter(|path| !path.is_empty())
                .map(str::to_owned),
        );
    }
    Ok(changed.into_iter().collect())
}

/// Computes or captures current WorkspaceState for a given repository.
pub async fn compute_workspace_state(
    repo_path: &Path,
    baseline_revision: &str,
) -> Result<WorkspaceState> {
    ensure!(
        repo_path.is_dir(),
        "workspace repository directory is missing"
    );
    if repo_path.join(".git").exists() {
        let head = git_output(repo_path, &["rev-parse", "--verify", "HEAD"]).await?;
        let head_revision = String::from_utf8(head)
            .context("GIT_WORKSPACE_STATE_FAILED: HEAD is not UTF-8")?
            .trim()
            .to_string();
        ensure!(
            !head_revision.is_empty(),
            "GIT_WORKSPACE_STATE_FAILED: empty HEAD"
        );
        let tracked_diff = git_output(
            repo_path,
            &[
                "diff",
                "--binary",
                "--no-ext-diff",
                "--full-index",
                "--no-renames",
                baseline_revision,
                "--",
            ],
        )
        .await?;
        let untracked = git_untracked_paths(repo_path).await?;
        let candidate_digest = hash_candidate(repo_path, &tracked_diff, &untracked)?;
        Ok(WorkspaceState::compute_candidate_v2(
            baseline_revision,
            &head_revision,
            &candidate_digest,
        ))
    } else {
        let paths = non_git_candidate_paths(repo_path)?;
        let candidate_digest = hash_candidate(repo_path, &[], &paths)?;
        Ok(WorkspaceState::compute_candidate_v2(
            baseline_revision,
            "NON_GIT",
            &candidate_digest,
        ))
    }
}

async fn git_output(repo_path: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = crate::tool_surface::safe_git_command(repo_path, args)
        .output()
        .await
        .context("GIT_WORKSPACE_STATE_FAILED: start git")?;
    ensure!(
        output.status.success(),
        "GIT_WORKSPACE_STATE_FAILED: git returned {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

async fn git_untracked_paths(repo_path: &Path) -> Result<Vec<PathBuf>> {
    let output = git_output(
        repo_path,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )
    .await?;
    let mut paths: Vec<PathBuf> = output
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| PathBuf::from(std::ffi::OsStr::from_bytes(path)))
        .collect();
    paths.sort();
    Ok(paths)
}

async fn review_candidate_diff(repo_path: &Path, baseline_revision: &str) -> Result<String> {
    let mut diff = if repo_path.join(".git").exists() {
        let mut diff = git_output(
            repo_path,
            &[
                "diff",
                "--binary",
                "--no-ext-diff",
                "--full-index",
                "--no-renames",
                baseline_revision,
                "--",
            ],
        )
        .await
        .context("REVIEW_ERROR: tracked diff failed")?;
        for relative in git_untracked_paths(repo_path).await? {
            let output = crate::tool_surface::safe_git_command(
                repo_path,
                &[
                    "diff",
                    "--no-index",
                    "--binary",
                    "--no-ext-diff",
                    "--full-index",
                    "--",
                    "/dev/null",
                ],
            )
            .arg(&relative)
            .output()
            .await
            .context("REVIEW_ERROR: start untracked file diff")?;
            ensure!(
                output.status.code() == Some(1) && !output.stdout.is_empty(),
                "REVIEW_ERROR: untracked diff failed for {}: {}",
                relative.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
            diff.extend(output.stdout);
        }
        diff
    } else {
        let mut inventory = Vec::new();
        for relative in non_git_candidate_paths(repo_path)? {
            let label = relative
                .to_str()
                .context("REVIEW_ERROR: non-UTF-8 candidate path")?;
            let path = repo_path.join(&relative);
            let metadata = std::fs::symlink_metadata(&path)?;
            let bytes = if metadata.file_type().is_symlink() {
                std::fs::read_link(&path)?.as_os_str().as_bytes().to_vec()
            } else if metadata.is_file() {
                std::fs::read(&path)?
            } else {
                bail!("REVIEW_ERROR: unsupported candidate file type: {label}");
            };
            let content = match String::from_utf8(bytes.clone()) {
                Ok(text) => text,
                Err(_) => format!("[binary hex: {}]", hex::encode(bytes)),
            };
            inventory.extend(format!("\n=== candidate file: {label} ===\n{content}\n").as_bytes());
        }
        inventory
    };
    ensure!(!diff.is_empty(), "REVIEW_ERROR: candidate diff is empty");
    String::from_utf8(std::mem::take(&mut diff))
        .context("REVIEW_ERROR: candidate diff is not UTF-8")
}

fn non_git_candidate_paths(repo_path: &Path) -> Result<Vec<PathBuf>> {
    fn visit(repo_path: &Path, relative: &Path, paths: &mut Vec<PathBuf>) -> Result<()> {
        for entry in std::fs::read_dir(repo_path.join(relative))? {
            let entry = entry?;
            let child = relative.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                visit(repo_path, &child, paths)?;
            } else {
                paths.push(child);
            }
        }
        Ok(())
    }
    let mut paths = Vec::new();
    visit(repo_path, Path::new(""), &mut paths)?;
    paths.sort();
    Ok(paths)
}

fn hash_candidate(repo_path: &Path, tracked_diff: &[u8], paths: &[PathBuf]) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(b"orbit-workspace-candidate-v2\0");
    hasher.update((tracked_diff.len() as u64).to_le_bytes());
    hasher.update(tracked_diff);
    for relative in paths {
        ensure!(
            relative
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_))),
            "candidate path is not confined to repository"
        );
        let name = relative.as_os_str().as_bytes();
        hasher.update((name.len() as u64).to_le_bytes());
        hasher.update(name);
        let path = repo_path.join(relative);
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("read candidate metadata for {}", relative.display()))?;
        let (kind, bytes) = if metadata.file_type().is_symlink() {
            (
                b's',
                std::fs::read_link(&path)?.as_os_str().as_bytes().to_vec(),
            )
        } else if metadata.is_file() {
            (
                b'f',
                std::fs::read(&path)
                    .with_context(|| format!("read candidate file {}", relative.display()))?,
            )
        } else {
            bail!("unsupported candidate file type: {}", relative.display());
        };
        hasher.update([kind]);
        hasher.update((metadata.permissions().mode() & 0o7777).to_le_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Simulated role executor for deterministic unit and qualification testing.
pub struct SimulatedRoleExecutor {
    pub plan_response: Mutex<Option<String>>,
    pub impl_response: Mutex<Option<String>>,
    pub review_response: Mutex<Option<String>>,
    pub repair_response: Mutex<Option<String>>,
    pub recorded_roles: Mutex<Vec<String>>,
}

impl Default for SimulatedRoleExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl SimulatedRoleExecutor {
    pub fn new() -> Self {
        Self {
            plan_response: Mutex::new(None),
            impl_response: Mutex::new(None),
            review_response: Mutex::new(None),
            repair_response: Mutex::new(None),
            recorded_roles: Mutex::new(Vec::new()),
        }
    }

    pub fn with_approval() -> Self {
        let exec = Self::new();
        *exec.plan_response.lock().unwrap() = Some(format!(
            "{}\n{}\n{}",
            ORBIT_HANDOFF_START,
            serde_json::to_string_pretty(&PlanHandoff {
                summary: "Standard implementation plan".into(),
                affected_areas: vec!["core".into()],
                implementation_steps: vec!["step 1".into()],
                expected_files: vec!["test.rs".into()],
                risks: vec![],
                verification_notes: vec![],
                open_questions: vec![],
            })
            .unwrap(),
            ORBIT_HANDOFF_END
        ));

        *exec.impl_response.lock().unwrap() = Some(format!(
            "{}\n{}\n{}",
            ORBIT_HANDOFF_START,
            serde_json::to_string_pretty(&ImplementationHandoff {
                summary: "Implemented planned changes".into(),
                changed_files: vec!["test.rs".into()],
                tests_added_or_modified: vec!["test_main".into()],
                exploratory_commands: vec![],
                known_limitations: vec![],
                verification_notes: vec![],
            })
            .unwrap(),
            ORBIT_HANDOFF_END
        ));

        *exec.review_response.lock().unwrap() = Some(format!(
            "{}\n{}\n{}",
            ORBIT_HANDOFF_START,
            serde_json::to_string_pretty(&ReviewDecision {
                decision: ReviewDecisionStatus::Approve,
                summary: "Code and verification passed review".into(),
                findings: vec![],
                requested_changes: vec![],
                suggested_additional_checks: vec![],
            })
            .unwrap(),
            ORBIT_HANDOFF_END
        ));

        *exec.repair_response.lock().unwrap() = Some(format!(
            "{}\n{}\n{}",
            ORBIT_HANDOFF_START,
            serde_json::to_string_pretty(&ImplementationHandoff {
                summary: "Repaired per review / failure feedback".into(),
                changed_files: vec!["test.rs".into()],
                tests_added_or_modified: vec![],
                exploratory_commands: vec![],
                known_limitations: vec![],
                verification_notes: vec![],
            })
            .unwrap(),
            ORBIT_HANDOFF_END
        ));

        exec
    }
}

#[async_trait::async_trait]
impl RoleAgentExecutor for SimulatedRoleExecutor {
    async fn execute_role(
        &self,
        _pool: &PgPool,
        _wf_run: &WorkflowRun,
        _role_exec: &RoleExecution,
        role: &RoleDefinition,
        _target: &ResolvedExecutionTarget,
        _task_text: &str,
        repo_path: &Path,
        _input_handoff: Option<&HandoffArtifact>,
        _cancellation: tokio::sync::watch::Receiver<bool>,
    ) -> Result<RoleExecutionOutcome> {
        self.recorded_roles
            .lock()
            .unwrap()
            .push(role.role_id.clone());

        let raw_output = match role.role_id.as_str() {
            "planner" => {
                let guard = self.plan_response.lock().unwrap();
                guard.clone().unwrap_or_else(|| {
                    format!(
                        "{}\n{}\n{}",
                        ORBIT_HANDOFF_START,
                        serde_json::to_string(&PlanHandoff {
                            summary: "Default plan".into(),
                            affected_areas: vec![],
                            implementation_steps: vec!["Implement task".into()],
                            expected_files: vec![],
                            risks: vec![],
                            verification_notes: vec![],
                            open_questions: vec![],
                        })
                        .unwrap(),
                        ORBIT_HANDOFF_END
                    )
                })
            }
            "implementer" => {
                // If implementer executes, simulate modifying a workspace file if in a temp directory
                if repo_path.exists() {
                    let dummy_file = repo_path.join("orbit_change.txt");
                    let _ = std::fs::write(
                        &dummy_file,
                        format!(
                            "change-{}",
                            SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_nanos()
                        ),
                    );
                }

                let is_repair = self.repair_response.lock().unwrap().is_some()
                    && self
                        .recorded_roles
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|r| *r == "implementer")
                        .count()
                        > 1;
                if is_repair {
                    let guard = self.repair_response.lock().unwrap();
                    guard.clone().unwrap_or_default()
                } else {
                    let guard = self.impl_response.lock().unwrap();
                    guard.clone().unwrap_or_else(|| {
                        format!(
                            "{}\n{}\n{}",
                            ORBIT_HANDOFF_START,
                            serde_json::to_string(&ImplementationHandoff {
                                summary: "Default implementation".into(),
                                changed_files: vec!["orbit_change.txt".into()],
                                tests_added_or_modified: vec![],
                                exploratory_commands: vec![],
                                known_limitations: vec![],
                                verification_notes: vec![],
                            })
                            .unwrap(),
                            ORBIT_HANDOFF_END
                        )
                    })
                }
            }
            "reviewer" => {
                let guard = self.review_response.lock().unwrap();
                guard.clone().unwrap_or_else(|| {
                    format!(
                        "{}\n{}\n{}",
                        ORBIT_HANDOFF_START,
                        serde_json::to_string(&ReviewDecision {
                            decision: ReviewDecisionStatus::Approve,
                            summary: "Default review approval".into(),
                            findings: vec![],
                            requested_changes: vec![],
                            suggested_additional_checks: vec![],
                        })
                        .unwrap(),
                        ORBIT_HANDOFF_END
                    )
                })
            }
            other => bail!("unexpected role id in simulator: {other}"),
        };

        Ok(RoleExecutionOutcome {
            raw_output,
            agent_execution_ids: vec![format!("sim-agent-{}", id())],
            termination_reason: Some("completed".into()),
        })
    }
}

fn list_files_recursively(base: &Path, dir: &Path, acc: &mut Vec<String>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.path());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                list_files_recursively(base, &path, acc);
            } else if path.is_file() && path.strip_prefix(base).is_ok() {
                let rel = path.strip_prefix(base).unwrap();
                acc.push(rel.to_string_lossy().into_owned());
            }
        }
    }
}

fn build_role_prompt(
    role: &RoleDefinition,
    task_text: &str,
    repo_path: &Path,
    base_revision: &str,
    input_handoff: Option<&HandoffArtifact>,
    git_diff: Option<&str>,
) -> String {
    let mut doc_files = Vec::new();
    let docs_dir = repo_path.join("docs");
    if docs_dir.is_dir() {
        list_files_recursively(repo_path, &docs_dir, &mut doc_files);
    }
    let docs_manifest = if doc_files.is_empty() {
        String::new()
    } else {
        format!(
            "\n            EXISTING DOCUMENTATION FILES:\n            {}\n",
            doc_files.join("\n            ")
        )
    };

    let prompt = match role.role_id.as_str() {
        "planner" => format!(
            "You are the PLANNER role in an Orbit automated software change workflow.\n            Your responsibility is to analyze the task, inspect the repository using read-only tools, and produce a clear, structured implementation plan.\n\n            TASK OBJECTIVE:\n{task_text}\n\n            REPOSITORY CONTEXT:\n            Repository Path: {repo_path}\n            Base Revision: {base_revision}{docs_manifest}\n            WORKSPACE PERMISSIONS:\n            You have READ-ONLY workspace access. You can inspect the repository using:\n            - fs/read_text_file (or read_file): read file content\n            - fs/list_directory: inspect workspace directory entries\n            - fs/find_path: search for files matching patterns\n            - search/grep: regex or text search across files\n            - git/status, git/diff, git/show: inspect git working tree and commit history\n            You CANNOT write or edit files, and CANNOT create terminals.\n\n            INSTRUCTIONS:\n            1. Inspect existing files, search patterns, and repository structure using the read-only tools.\n            2. Formulate a concrete step-by-step implementation plan.\n            3. You MUST end your response with a structured JSON plan handoff block inside the exact delimiters:\n            <<<ORBIT_HANDOFF_START>>>\n            {{\n              \"summary\": \"Concise summary of the plan\",\n              \"affected_areas\": [\"area1\", \"area2\"],\n              \"implementation_steps\": [\"step 1\", \"step 2\"],\n              \"expected_files\": [\"docs/file1.md\"],\n              \"risks\": [],\n              \"verification_notes\": [\"verification instructions\"],\n              \"open_questions\": []\n            }}\n            <<<ORBIT_HANDOFF_END>>>\n",
            repo_path = repo_path.display(),
            base_revision = base_revision,
            task_text = task_text,
            docs_manifest = docs_manifest,
        ),
        "implementer" => {
            let plan_summary = input_handoff
                .map(|h| h.structured_payload.to_string())
                .unwrap_or_else(|| "No prior plan provided.".to_string());
            format!(
                "You are the IMPLEMENTER role in an Orbit automated software change workflow.\n                Your responsibility is to execute the implementation plan by modifying project files and verifying your work.\n\n                TASK OBJECTIVE:\n{task_text}\n\n                PLANNER SPECIFICATION:\n{plan_summary}\n\n                REPOSITORY CONTEXT:\n                Repository Path: {repo_path}\n                Base Revision: {base_revision}{docs_manifest}\n                WORKSPACE PERMISSIONS:\n                You have FULL READ-WRITE coding agent workspace access. Tools available to you:\n                - fs/read_text_file: read file contents\n                - fs/write_text_file: write complete file contents\n                - fs/edit_file: perform targeted text replacements (old_text -> new_text, replace_all)\n                - fs/list_directory: list directory contents\n                - fs/find_path: search workspace file paths by pattern\n                - fs/create_directory: create a new directory\n                - fs/move: move or rename files/directories\n                - fs/copy: copy files or directories\n                - fs/delete_file: remove a single file\n                - fs/delete_directory: remove a directory\n                - search/grep: ripgrep workspace code\n                - git/status, git/diff, git/show: inspect git status, diffs, and commits\n                - terminal/create, terminal/output, terminal/wait_for_exit, terminal/kill, terminal/release: run tests or commands\n\n                INSTRUCTIONS:\n                1. Implement all required changes and directory reorganization per the planner specification.\n                2. Use fs/edit_file for surgical modifications and fs/write_text_file for new files.\n                3. You MUST end your response with a structured JSON implementation handoff block inside the exact delimiters:\n                <<<ORBIT_HANDOFF_START>>>\n                {{\n                  \"summary\": \"Concise summary of changes implemented\",\n                  \"changed_files\": [\"docs/file1.md\"],\n                  \"tests_added_or_modified\": [],\n                  \"exploratory_commands\": [],\n                  \"known_limitations\": [],\n                  \"verification_notes\": [\"self-verification details\"]\n                }}\n                <<<ORBIT_HANDOFF_END>>>\n",
                repo_path = repo_path.display(),
                base_revision = base_revision,
                task_text = task_text,
                plan_summary = plan_summary,
                docs_manifest = docs_manifest,
            )
        }
        "reviewer" => {
            let handoff_summary = input_handoff
                .map(|h| h.structured_payload.to_string())
                .unwrap_or_else(|| "No prior implementation handoff provided.".to_string());
            let diff_text = git_diff.unwrap_or("No diff recorded.");
            format!(
                "You are the REVIEWER role in an Orbit automated software change workflow.\n                Your responsibility is to review the code changes against the task objective and implementation handoff.\n\n                TASK OBJECTIVE:\n{task_text}\n\n                IMPLEMENTATION HANDOFF:\n{handoff_summary}\n\n                GIT DIFF:\n{diff_text}\n\n                WORKSPACE PERMISSIONS:\n                You have READ-ONLY workspace access. You can inspect files using fs/read_text_file.\n                You CANNOT write files.\n\n                INSTRUCTIONS:\n                1. Carefully review the git diff and verify that the changes satisfy the task without regressions.\n                2. Decide whether to APPROVE or request CHANGES_REQUESTED.\n                3. You MUST end your response with a structured JSON review decision block inside the exact delimiters:\n                <<<ORBIT_HANDOFF_START>>>\n                {{\n                  \"decision\": \"APPROVE\",\n                  \"summary\": \"Review rationale and summary\",\n                  \"findings\": [\n                    {{\n                      \"category\": \"documentation\",\n                      \"severity\": \"medium\",\n                      \"path\": \"docs/README.md\",\n                      \"explanation\": \"Clear description of finding\",\n                      \"requested_change\": \"Specific change required\"\n                    }}\n                  ],\n                  \"requested_changes\": [\"specific change 1\"],\n                  \"suggested_additional_checks\": []\n                }}\n                <<<ORBIT_HANDOFF_END>>>\n                Note: decision must be either APPROVE, CHANGES_REQUESTED, or BLOCKED.\n",
                task_text = task_text,
                handoff_summary = handoff_summary,
                diff_text = diff_text,
            )
        }
        _ => format!(
            "Execute role {role_id} for task: {task_text}\n            You MUST end your response with a structured JSON handoff inside <<<ORBIT_HANDOFF_START>>> and <<<ORBIT_HANDOFF_END>>>.\n",
            role_id = role.role_id,
            task_text = task_text,
        ),
    };
    prompt.replace(
        "- terminal/create, terminal/output, terminal/wait_for_exit, terminal/kill, terminal/release: run tests or commands",
        "- CLI workflow terminal execution is unavailable until a confined terminal owner is qualified",
    )
}

pub struct AcpTurnState<'a> {
    pub repo_path: &'a Path,
    pub workspace_access: WorkspaceAccess,
    pub role_id: Option<String>,
    pub workspace_identity: Option<String>,
    pub tool_call_limit: u64,
    pub agent_output: String,
    pub tool_calls: u64,
    pub tool_successes: u64,
    pub tool_failures: u64,
    pub tool_counts: BTreeMap<String, u64>,
    pub terminals: BTreeMap<String, std::sync::Arc<crate::tool_surface::AgentTerminal>>,
    pub wf_attempt_id: Option<String>,
    pub role_exec_id: Option<String>,
    pub pool: Option<&'a PgPool>,
}

impl<'a> AcpTurnState<'a> {
    pub fn new(repo_path: &'a Path, workspace_access: WorkspaceAccess) -> Self {
        Self {
            repo_path,
            workspace_access,
            role_id: None,
            workspace_identity: None,
            tool_call_limit: 64,
            agent_output: String::new(),
            tool_calls: 0,
            tool_successes: 0,
            tool_failures: 0,
            tool_counts: BTreeMap::new(),
            terminals: BTreeMap::new(),
            wf_attempt_id: None,
            role_exec_id: None,
            pool: None,
        }
    }
}

fn extract_text_from_json(val: &serde_json::Value, out: &mut String) {
    match val {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(s)) = map.get("text") {
                out.push_str(s);
            } else if let Some(serde_json::Value::String(s)) = map.get("delta") {
                out.push_str(s);
            }
            for (k, v) in map {
                if k != "text" && k != "delta" {
                    extract_text_from_json(v, out);
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                extract_text_from_json(v, out);
            }
        }
        _ => {}
    }
}

pub async fn handle_acp_message(
    wire: &mut Wire,
    state: &mut AcpTurnState<'_>,
    message: serde_json::Value,
) -> Result<()> {
    if let Some(method) = message.get("method").and_then(|m| m.as_str()) {
        if method == "session/update" {
            if let Some(params) = message.get("params") {
                if let Some(update) = params.get("update") {
                    extract_text_from_json(update, &mut state.agent_output);
                } else {
                    extract_text_from_json(params, &mut state.agent_output);
                }
            }
            return Ok(());
        }

        let req_id = message
            .get("id")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        let canonical = crate::tool_surface::CanonicalToolName::from_wire(method);
        let Some(tool) = canonical else {
            if let Some(id) = message.get("id").cloned() {
                wire.response_error(
                    id,
                    -32601,
                    &format!("{}: {method}", crate::tool_surface::ERR_UNSUPPORTED_TOOL),
                )
                .await?;
            }
            return Ok(());
        };

        state.tool_calls = state.tool_calls.saturating_add(1);
        *state
            .tool_counts
            .entry(tool.legacy_name().into())
            .or_insert(0) += 1;
        *state.tool_counts.entry(tool.as_str().into()).or_insert(0) += 1;

        let params = message
            .get("params")
            .cloned()
            .unwrap_or(serde_json::json!({}));

        if matches!(
            tool,
            crate::tool_surface::CanonicalToolName::TerminalCreate
                | crate::tool_surface::CanonicalToolName::TerminalOutput
                | crate::tool_surface::CanonicalToolName::TerminalWaitForExit
                | crate::tool_surface::CanonicalToolName::TerminalKill
                | crate::tool_surface::CanonicalToolName::TerminalRelease
        ) {
            state.tool_failures += 1;
            wire.response_error(req_id, -32603, ERR_CLI_WORKFLOW_TERMINAL_DISABLED)
                .await?;
            return Ok(());
        }

        let meta = match crate::tool_surface::authorize_repository_tool(
            tool,
            state.role_id.as_deref(),
            state.workspace_access,
            state.repo_path,
            state.workspace_identity.as_deref(),
            state.tool_calls,
            state.tool_call_limit,
        ) {
            Ok(metadata) => metadata,
            Err(error) => {
                state.tool_failures += 1;
                wire.response_error(req_id, -32603, &error.to_string())
                    .await?;
                return Ok(());
            }
        };
        wire.set_response_limit(meta.max_output_bytes);

        // Mutation lock enforcement for mutating operations
        if meta.requires_mutation_lock {
            let (Some(pool), Some(att_id), Some(role_id)) =
                (state.pool, &state.wf_attempt_id, &state.role_exec_id)
            else {
                state.tool_failures += 1;
                wire.response_error(
                    req_id,
                    -32603,
                    crate::tool_surface::ERR_MUTATION_LOCK_REQUIRED,
                )
                .await?;
                return Ok(());
            };
            let wf_store = WorkflowStore::new(pool.clone());
            let lock_held = wf_store
                .check_workspace_mutation_lock(att_id, role_id)
                .await
                .unwrap_or(false);
            if !lock_held {
                state.tool_failures += 1;
                wire.response_error(
                    req_id,
                    -32603,
                    crate::tool_surface::ERR_MUTATION_LOCK_REQUIRED,
                )
                .await?;
                return Ok(());
            }
        }

        let successes_before = state.tool_successes;
        let failures_before = state.tool_failures;
        let timeout_request_id = req_id.clone();
        let operation = async {
            match tool {
                crate::tool_surface::CanonicalToolName::FsReadTextFile => {
                    let rel_path_str = params.get("path").and_then(|p| p.as_str()).unwrap_or("");
                    match crate::fs_tools::read_text_confined(state.repo_path, rel_path_str) {
                        Ok(content) => {
                            let bounded_content = if content.len() > 65536 {
                                let mut end = 65536;
                                while end > 0 && !content.is_char_boundary(end) {
                                    end -= 1;
                                }
                                &content[..end]
                            } else {
                                &content
                            };
                            state.tool_successes += 1;
                            wire.response_ok(
                                req_id,
                                serde_json::json!({ "content": bounded_content }),
                            )
                            .await?;
                        }
                        Err(e) => {
                            state.tool_failures += 1;
                            wire.response_ok(
                            req_id,
                            serde_json::json!({ "content": format!("Error reading file: {e}") }),
                        )
                        .await?;
                        }
                    }
                }
                crate::tool_surface::CanonicalToolName::FsWriteTextFile => {
                    let rel_path_str = params.get("path").and_then(|p| p.as_str()).unwrap_or("");
                    let content = params.get("content").and_then(|p| p.as_str()).unwrap_or("");

                    match crate::fs_tools::write_text_confined(
                        state.repo_path,
                        rel_path_str,
                        content,
                    ) {
                        Ok(()) => {
                            state.tool_successes += 1;
                            wire.response_ok(req_id, serde_json::json!({})).await?;
                        }
                        Err(e) => {
                            state.tool_failures += 1;
                            wire.response_error(
                                req_id,
                                -32603,
                                &format!("failed to write file: {e}"),
                            )
                            .await?;
                        }
                    }
                }
                crate::tool_surface::CanonicalToolName::FsEditFile => {
                    let path_str = params.get("path").and_then(|p| p.as_str()).unwrap_or("");
                    let old_text = params
                        .get("old_text")
                        .and_then(|p| p.as_str())
                        .unwrap_or("");
                    let new_text = params
                        .get("new_text")
                        .and_then(|p| p.as_str())
                        .unwrap_or("");
                    let replace_all = params
                        .get("replace_all")
                        .and_then(|p| p.as_bool())
                        .unwrap_or(false);

                    match crate::tool_surface::edit_file(
                        state.repo_path,
                        path_str,
                        old_text,
                        new_text,
                        replace_all,
                    ) {
                        Ok(res) => {
                            state.tool_successes += 1;
                            wire.response_ok(req_id, serde_json::to_value(res)?).await?;
                        }
                        Err(e) => {
                            state.tool_failures += 1;
                            wire.response_error(req_id, -32603, &e.to_string()).await?;
                        }
                    }
                }
                crate::tool_surface::CanonicalToolName::FsListDirectory => {
                    let path_str = params.get("path").and_then(|p| p.as_str()).unwrap_or(".");
                    let recursive = params
                        .get("recursive")
                        .and_then(|p| p.as_bool())
                        .unwrap_or(false);
                    let max_entries = params
                        .get("max_entries")
                        .and_then(|p| p.as_u64())
                        .unwrap_or(100) as usize;
                    let include_hidden = params
                        .get("include_hidden")
                        .and_then(|p| p.as_bool())
                        .unwrap_or(false);

                    match crate::tool_surface::list_directory(
                        state.repo_path,
                        path_str,
                        recursive,
                        max_entries,
                        include_hidden,
                    ) {
                        Ok(res) => {
                            state.tool_successes += 1;
                            wire.response_ok(req_id, serde_json::to_value(res)?).await?;
                        }
                        Err(e) => {
                            state.tool_failures += 1;
                            wire.response_error(req_id, -32603, &e.to_string()).await?;
                        }
                    }
                }
                crate::tool_surface::CanonicalToolName::FsFindPath => {
                    let pattern = params
                        .get("pattern")
                        .and_then(|p| p.as_str())
                        .unwrap_or("*");
                    let path_str = params.get("path").and_then(|p| p.as_str());
                    let max_results = params
                        .get("max_results")
                        .and_then(|p| p.as_u64())
                        .unwrap_or(100) as usize;

                    match crate::tool_surface::find_path(
                        state.repo_path,
                        path_str,
                        pattern,
                        &[],
                        &[],
                        max_results,
                    ) {
                        Ok(res) => {
                            state.tool_successes += 1;
                            wire.response_ok(req_id, serde_json::to_value(res)?).await?;
                        }
                        Err(e) => {
                            state.tool_failures += 1;
                            wire.response_error(req_id, -32603, &e.to_string()).await?;
                        }
                    }
                }
                crate::tool_surface::CanonicalToolName::SearchGrep => {
                    let query = params.get("query").and_then(|p| p.as_str()).unwrap_or("");
                    let path_str = params.get("path").and_then(|p| p.as_str());
                    let case_sensitive = params
                        .get("case_sensitive")
                        .and_then(|p| p.as_bool())
                        .unwrap_or(true);
                    let is_regex = params
                        .get("is_regex")
                        .and_then(|p| p.as_bool())
                        .unwrap_or(false);
                    let max_matches = params
                        .get("max_matches")
                        .and_then(|p| p.as_u64())
                        .unwrap_or(100) as usize;

                    match crate::tool_surface::search_grep(
                        state.repo_path,
                        path_str,
                        query,
                        case_sensitive,
                        is_regex,
                        &[],
                        &[],
                        max_matches,
                        0,
                    ) {
                        Ok(res) => {
                            state.tool_successes += 1;
                            wire.response_ok(req_id, serde_json::to_value(res)?).await?;
                        }
                        Err(e) => {
                            state.tool_failures += 1;
                            wire.response_error(req_id, -32603, &e.to_string()).await?;
                        }
                    }
                }
                crate::tool_surface::CanonicalToolName::FsCreateDirectory => {
                    let path_str = params.get("path").and_then(|p| p.as_str()).unwrap_or("");
                    let recursive = params
                        .get("recursive")
                        .and_then(|p| p.as_bool())
                        .unwrap_or(true);

                    match crate::fs_tools::create_directory(state.repo_path, path_str, recursive) {
                        Ok(_) => {
                            state.tool_successes += 1;
                            wire.response_ok(
                            req_id,
                            serde_json::json!({
                                "success": true,
                                "path": path_str,
                                "message": format!("Directory {} created successfully.", path_str),
                            }),
                        )
                        .await?;
                        }
                        Err(e) => {
                            state.tool_failures += 1;
                            wire.response_ok(
                                req_id,
                                serde_json::json!({
                                    "success": false,
                                    "path": path_str,
                                    "error": format!("{e:#}"),
                                }),
                            )
                            .await?;
                        }
                    }
                }
                crate::tool_surface::CanonicalToolName::FsMove => {
                    let source_str = params.get("source").and_then(|p| p.as_str()).unwrap_or("");
                    let destination_str = params
                        .get("destination")
                        .and_then(|p| p.as_str())
                        .unwrap_or("");

                    match crate::fs_tools::move_path(state.repo_path, source_str, destination_str) {
                        Ok(_) => {
                            state.tool_successes += 1;
                            wire.response_ok(req_id, serde_json::json!({
                            "success": true,
                            "source": source_str,
                            "destination": destination_str,
                            "message": format!("Moved {} to {} successfully.", source_str, destination_str),
                        })).await?;
                        }
                        Err(e) => {
                            state.tool_failures += 1;
                            wire.response_ok(
                                req_id,
                                serde_json::json!({
                                    "success": false,
                                    "source": source_str,
                                    "destination": destination_str,
                                    "error": format!("{e:#}"),
                                }),
                            )
                            .await?;
                        }
                    }
                }
                crate::tool_surface::CanonicalToolName::FsCopy => {
                    let source_str = params.get("source").and_then(|p| p.as_str()).unwrap_or("");
                    let destination_str = params
                        .get("destination")
                        .and_then(|p| p.as_str())
                        .unwrap_or("");
                    let recursive = params
                        .get("recursive")
                        .and_then(|p| p.as_bool())
                        .unwrap_or(false);

                    match crate::tool_surface::copy_path(
                        state.repo_path,
                        source_str,
                        destination_str,
                        recursive,
                    ) {
                        Ok(res) => {
                            state.tool_successes += 1;
                            wire.response_ok(req_id, serde_json::to_value(res)?).await?;
                        }
                        Err(e) => {
                            state.tool_failures += 1;
                            wire.response_error(req_id, -32603, &e.to_string()).await?;
                        }
                    }
                }
                crate::tool_surface::CanonicalToolName::FsDeleteFile => {
                    let path_str = params.get("path").and_then(|p| p.as_str()).unwrap_or("");

                    match crate::fs_tools::delete_file(state.repo_path, path_str) {
                        Ok(_) => {
                            state.tool_successes += 1;
                            wire.response_ok(
                                req_id,
                                serde_json::json!({
                                    "success": true,
                                    "path": path_str,
                                    "message": format!("File {} deleted successfully.", path_str),
                                }),
                            )
                            .await?;
                        }
                        Err(e) => {
                            state.tool_failures += 1;
                            wire.response_ok(
                                req_id,
                                serde_json::json!({
                                    "success": false,
                                    "path": path_str,
                                    "error": format!("{e:#}"),
                                }),
                            )
                            .await?;
                        }
                    }
                }
                crate::tool_surface::CanonicalToolName::FsDeleteDirectory => {
                    let path_str = params.get("path").and_then(|p| p.as_str()).unwrap_or("");
                    let recursive = params
                        .get("recursive")
                        .and_then(|p| p.as_bool())
                        .unwrap_or(false);

                    match crate::fs_tools::delete_directory(state.repo_path, path_str, recursive) {
                        Ok(_) => {
                            state.tool_successes += 1;
                            wire.response_ok(
                            req_id,
                            serde_json::json!({
                                "success": true,
                                "path": path_str,
                                "message": format!("Directory {} deleted successfully.", path_str),
                            }),
                        )
                        .await?;
                        }
                        Err(e) => {
                            state.tool_failures += 1;
                            wire.response_ok(
                                req_id,
                                serde_json::json!({
                                    "success": false,
                                    "path": path_str,
                                    "error": format!("{e:#}"),
                                }),
                            )
                            .await?;
                        }
                    }
                }
                crate::tool_surface::CanonicalToolName::GitStatus => {
                    let path_str = params.get("path").and_then(|p| p.as_str());

                    match crate::tool_surface::git_status(state.repo_path, path_str).await {
                        Ok(res) => {
                            state.tool_successes += 1;
                            wire.response_ok(req_id, serde_json::to_value(res)?).await?;
                        }
                        Err(e) => {
                            state.tool_failures += 1;
                            wire.response_error(req_id, -32603, &e.to_string()).await?;
                        }
                    }
                }
                crate::tool_surface::CanonicalToolName::GitDiff => {
                    let base = params.get("base").and_then(|p| p.as_str());
                    let path_str = params.get("path").and_then(|p| p.as_str());
                    let stat_only = params
                        .get("stat_only")
                        .and_then(|p| p.as_bool())
                        .unwrap_or(false);
                    let context_lines = params
                        .get("context_lines")
                        .and_then(|p| p.as_u64())
                        .map(|v| v as u32);

                    match crate::tool_surface::git_diff(
                        state.repo_path,
                        base,
                        path_str,
                        context_lines,
                        stat_only,
                        65536,
                    )
                    .await
                    {
                        Ok(res) => {
                            state.tool_successes += 1;
                            wire.response_ok(req_id, serde_json::to_value(res)?).await?;
                        }
                        Err(e) => {
                            state.tool_failures += 1;
                            wire.response_error(req_id, -32603, &e.to_string()).await?;
                        }
                    }
                }
                crate::tool_surface::CanonicalToolName::GitShow => {
                    let revision = params
                        .get("revision")
                        .and_then(|p| p.as_str())
                        .unwrap_or("HEAD");
                    let path_str = params.get("path").and_then(|p| p.as_str());

                    match crate::tool_surface::git_show(state.repo_path, revision, path_str, 65536)
                        .await
                    {
                        Ok(res) => {
                            state.tool_successes += 1;
                            wire.response_ok(req_id, serde_json::to_value(res)?).await?;
                        }
                        Err(e) => {
                            state.tool_failures += 1;
                            wire.response_error(req_id, -32603, &e.to_string()).await?;
                        }
                    }
                }
                crate::tool_surface::CanonicalToolName::TerminalCreate => {
                    let raw_cmd = params
                        .get("command")
                        .and_then(|v| v.as_str())
                        .unwrap_or("sh");
                    let cwd_str = params.get("cwd").and_then(|v| v.as_str()).unwrap_or(".");
                    let cwd =
                        match crate::fs_tools::confine_path(state.repo_path, cwd_str, true, true) {
                            Ok(p) => p,
                            Err(e) => {
                                state.tool_failures += 1;
                                wire.response_error(
                                    req_id,
                                    -32603,
                                    &format!(
                                        "{}: {e}",
                                        crate::tool_surface::ERR_PATH_OUTSIDE_WORKSPACE
                                    ),
                                )
                                .await?;
                                return Ok(());
                            }
                        };
                    let (cmd_bin, cmd_args) =
                        if let Some(arr) = params.get("args").and_then(|v| v.as_array()) {
                            let mut v = Vec::new();
                            for a in arr {
                                if let Some(s) = a.as_str() {
                                    v.push(s.to_string());
                                }
                            }
                            (raw_cmd.to_string(), v)
                        } else {
                            (
                                "sh".to_string(),
                                vec!["-c".to_string(), raw_cmd.to_string()],
                            )
                        };
                    let output_byte_limit = params
                        .get("output_byte_limit")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(65536)
                        .min(65536) as usize;
                    match crate::tool_surface::AgentTerminal::spawn(
                        &cwd,
                        &cmd_bin,
                        &cmd_args,
                        output_byte_limit,
                    ) {
                        Ok(term) => {
                            let tid = format!("term-{}", crate::model::id());
                            state
                                .terminals
                                .insert(tid.clone(), std::sync::Arc::new(term));
                            state.tool_successes += 1;
                            wire.response_ok(req_id, serde_json::json!({ "terminalId": tid }))
                                .await?;
                        }
                        Err(e) => {
                            state.tool_failures += 1;
                            wire.response_error(req_id, -32603, &e.to_string()).await?;
                        }
                    }
                }
                crate::tool_surface::CanonicalToolName::TerminalOutput => {
                    let tid = params
                        .get("terminalId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if let Some(term) = state.terminals.get(tid) {
                        let out = term.output();
                        let exit_status =
                            out.exit_code.map(|c| serde_json::json!({ "exitCode": c }));
                        state.tool_successes += 1;
                        wire.response_ok(
                            req_id,
                            serde_json::json!({
                                "output": out.text(),
                                "truncated": out.truncated,
                                "exitStatus": exit_status,
                            }),
                        )
                        .await?;
                    } else {
                        state.tool_failures += 1;
                        wire.response_error(
                            req_id,
                            -32603,
                            crate::tool_surface::ERR_PROCESS_NOT_FOUND,
                        )
                        .await?;
                    }
                }
                crate::tool_surface::CanonicalToolName::TerminalWaitForExit => {
                    let tid = params
                        .get("terminalId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let term_opt = state.terminals.get(tid).cloned();
                    if let Some(term) = term_opt {
                        match term.wait_for_exit(Duration::from_secs(300)).await {
                            Ok(code) => {
                                state.tool_successes += 1;
                                wire.response_ok(
                                    req_id,
                                    serde_json::json!({
                                        "exitStatus": { "exitCode": code }
                                    }),
                                )
                                .await?;
                            }
                            Err(e) => {
                                state.tool_failures += 1;
                                wire.response_error(req_id, -32603, &e.to_string()).await?;
                            }
                        }
                    } else {
                        state.tool_failures += 1;
                        wire.response_error(
                            req_id,
                            -32603,
                            crate::tool_surface::ERR_PROCESS_NOT_FOUND,
                        )
                        .await?;
                    }
                }
                crate::tool_surface::CanonicalToolName::TerminalKill => {
                    let tid = params
                        .get("terminalId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if let Some(term) = state.terminals.get(tid) {
                        let _ = term.kill().await;
                        state.tool_successes += 1;
                        wire.response_ok(req_id, serde_json::json!({})).await?;
                    } else {
                        state.tool_failures += 1;
                        wire.response_error(
                            req_id,
                            -32603,
                            crate::tool_surface::ERR_PROCESS_NOT_FOUND,
                        )
                        .await?;
                    }
                }
                crate::tool_surface::CanonicalToolName::TerminalRelease => {
                    let tid = params
                        .get("terminalId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if let Some(term) = state.terminals.remove(tid) {
                        let _ = term.kill().await;
                        state.tool_successes += 1;
                        wire.response_ok(req_id, serde_json::json!({})).await?;
                    } else {
                        state.tool_failures += 1;
                        wire.response_error(
                            req_id,
                            -32603,
                            crate::tool_surface::ERR_PROCESS_NOT_FOUND,
                        )
                        .await?;
                    }
                }
            }
            Ok::<(), anyhow::Error>(())
        };
        match tokio::time::timeout(Duration::from_secs(meta.default_timeout_seconds), operation)
            .await
        {
            Ok(result) => result?,
            Err(_) => {
                state.tool_failures += 1;
                wire.response_error(
                    timeout_request_id,
                    -32603,
                    crate::tool_surface::ERR_COMMAND_TIMEOUT,
                )
                .await?;
            }
        }
        if wire.take_response_limit_hit() {
            if state.tool_successes > successes_before {
                state.tool_successes -= 1;
            }
            if state.tool_failures == failures_before {
                state.tool_failures += 1;
            }
        }
        Ok(())
    } else {
        Ok(())
    }
}

async fn acp_call(
    wire: &mut Wire,
    state: &mut AcpTurnState<'_>,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    let id = wire.request(method, params).await?;
    loop {
        let value = wire.read().await?;
        if value.get("method").is_some() {
            handle_acp_message(wire, state, value).await?;
        } else {
            return Wire::result(value, &id);
        }
    }
}

fn validate_role_credential_target(
    target: &ResolvedExecutionTarget,
    credential: &crate::credential_registry::Credential,
) -> Result<()> {
    let generation = target
        .credential_generation
        .context("CREDENTIAL_PIN_REQUIRED")?;
    ensure!(
        target.credential_id.as_deref() == Some(credential.reference.as_str())
            && credential.provider == target.provider
            && credential.generation == u64::from(generation)
            && credential.status == crate::credential_registry::CredentialStatus::Enrolled,
        "CREDENTIAL_GENERATION_CHANGED: selected credential reference, provider, status, or generation changed before execution"
    );
    Ok(())
}

struct RoleModelEvidence<'a> {
    requested: Option<&'a str>,
    configured: Option<&'a str>,
    observed: Option<&'a str>,
}

fn role_model_evidence<'a>(
    target: &'a ResolvedExecutionTarget,
    observed: Option<&'a str>,
) -> RoleModelEvidence<'a> {
    RoleModelEvidence {
        requested: target.requested_model.as_deref(),
        configured: target.resolved_model.as_deref(),
        observed,
    }
}

#[derive(Debug)]
struct SupervisorEvidence {
    exit_code: Option<i32>,
    cleanup_confirmed: bool,
    failure: Option<String>,
}

fn classify_supervisor_evidence(
    status: std::process::ExitStatus,
    cleanup: Result<i32>,
) -> SupervisorEvidence {
    use std::os::unix::process::ExitStatusExt;
    let exit_code = status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal));
    let mut failure = (!status.success()).then(|| format!("ROLE_SUPERVISOR_EXIT_FAILED: {status}"));
    let cleanup_confirmed = match cleanup {
        Ok(receipt_code) if Some(receipt_code) == exit_code => true,
        Ok(receipt_code) => {
            failure.get_or_insert_with(|| format!(
                "ROLE_CLEANUP_UNCONFIRMED: supervisor exit {exit_code:?} differs from receipt {receipt_code}"
            ));
            false
        }
        Err(error) => {
            failure.get_or_insert_with(|| format!("ROLE_CLEANUP_UNCONFIRMED: {error:#}"));
            false
        }
    };
    SupervisorEvidence {
        exit_code,
        cleanup_confirmed,
        failure,
    }
}

fn validate_role_turn_completion(output: &str, evidence: &SupervisorEvidence) -> Result<()> {
    if let Some(reason) = evidence.failure.as_deref() {
        bail!("{reason}");
    }
    ensure!(evidence.cleanup_confirmed, "ROLE_CLEANUP_UNCONFIRMED");
    ensure!(
        output.contains(ORBIT_HANDOFF_START) && output.contains(ORBIT_HANDOFF_END),
        "ROLE_OUTPUT_INVALID: missing structured handoff block"
    );
    Ok(())
}

async fn wait_cli_supervisor(
    child: &mut tokio::process::Child,
    wait_timeout: Duration,
    kill_timeout: Duration,
) -> Result<std::process::ExitStatus> {
    match tokio::time::timeout(wait_timeout, child.wait()).await {
        Ok(Ok(status)) => Ok(status),
        Ok(Err(error)) => {
            Err(UnconfirmedRoleCleanup(format!("supervisor wait failed: {error}")).into())
        }
        Err(_) => {
            if let Some(pid) = child.id() {
                unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            }
            child.start_kill().map_err(|error| {
                UnconfirmedRoleCleanup(format!("kill timed-out supervisor failed: {error}"))
            })?;
            tokio::time::timeout(kill_timeout, child.wait())
                .await
                .map_err(|_| UnconfirmedRoleCleanup("supervisor did not exit after kill".into()))?
                .map_err(|error| UnconfirmedRoleCleanup(format!("supervisor reap failed: {error}")))
                .map_err(Into::into)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_real_acp_turn(
    pool: &PgPool,
    wf_run: &WorkflowRun,
    role_exec: &RoleExecution,
    role: &RoleDefinition,
    target: &ResolvedExecutionTarget,
    task_text: &str,
    repo_path: &Path,
    input_handoff: Option<&HandoffArtifact>,
    mut cancellation: tokio::sync::watch::Receiver<bool>,
) -> Result<RoleExecutionOutcome> {
    let agent_exec_id = format!("acp-exec-{}", id());
    let started_at_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as i64;

    let cred_store = CredentialStore::new(pool);
    let credential_ref = target
        .credential_id
        .as_deref()
        .context("CREDENTIAL_PIN_REQUIRED")?;
    let credential = cred_store
        .get(credential_ref)
        .await?
        .context("target credential not found")?;
    validate_role_credential_target(target, &credential)?;

    let backend = LocalPrivateSecretBackend::default_for_operator()?;

    let scratch_dir = tempfile::Builder::new()
        .prefix("orbit-acp-role-")
        .tempdir()?;
    let auth_store_dir = scratch_dir.path().join("auth");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&auth_store_dir)?;
    let auth_store_dir = auth_store_dir.canonicalize()?;

    let runtime = if target.provider == "codex" {
        let secret_bytes = registered_auth_diagnostic(pool, &backend, &credential.reference)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "failed to stage Codex credentials for {}: {:?}",
                    credential.reference,
                    e
                )
            })?;
        let auth_file = auth_store_dir.join("auth.json");
        tokio::fs::write(&auth_file, secret_bytes.expose()).await?;
        std::fs::set_permissions(&auth_file, std::fs::Permissions::from_mode(0o600))?;

        use crate::codex_credential_enrollment as enrolled;
        let binding_name = "codex-role-v1";
        let launch = Launch {
            adapter: Adapter::Codex,
            image: enrolled::CODEX_IMAGE.into(),
            command: vec![enrolled::CODEX_BINARY.into(), "app-server".into()],
            agent_name: "orbit-codex-acp".into(),
            agent_version: "1".into(),
            binary_revision: enrolled::CODEX_VERSION.into(),
            cpu_millis: 1000,
            memory_mib: 512,
            network: AgentNetwork::Host,
        };
        let auth = Auth {
            source: "codex".into(),
            owner: credential.reference.clone(),
            account_class: "chatgpt".into(),
            mode: AuthMode::LocalSession,
        };
        let descriptor = Descriptor {
            agent_id: "codex".into(),
            agent_revision: crate::codex_bridge::REVISION.into(),
            launch_digest: launch.digest()?,
            protocol_version: 1,
            auth: auth.clone(),
            security_profile: SecurityProfile::Trusted,
            filesystem_policy: FilesystemPolicy::AttemptWorkspace,
            terminal_policy: TerminalPolicy::WorkspaceSupervisor,
            model_policy: ModelPolicy::Exact,
            accounting: Accounting::ExecutionOnly,
            max_limits: AcpLimits {
                prompt_turns: 1,
                broker_calls: 32,
                reported_tool_calls: 64,
                turn_timeout_seconds: 300,
                terminal_timeout_seconds: 30,
                terminal_runtime_seconds: 0,
                output_bytes: 524288,
            },
        };
        let mut files = BTreeMap::new();
        files.insert("auth.json".into(), enrolled::CODEX_AUTH_RELATIVE.into());
        let rt = Runtime {
            binding_name: binding_name.into(),
            binding: Binding {
                model: target
                    .resolved_model
                    .clone()
                    .or_else(|| Some("gpt-6-luna".into())),
                runtime: "agent.codex-role-v1".into(),
                tools: Default::default(),
                permissions: Vec::new(),
                max_budget: Budget {
                    tokens: None,
                    cost_microusd: None,
                    calls: 1,
                },
                max_delegations: 0,
                acp: Some(descriptor),
            },
            launch,
            auth: AuthStore {
                path: auth_store_dir.clone(),
                source: auth.source,
                owner: auth.owner,
                account_class: auth.account_class,
                files,
                scopes: Vec::new(),
            },
            reasoning_effort: None,
        };
        rt.validate()?;
        rt
    } else if target.provider == "antigravity" {
        let inspection = cred_store
            .inspect(&credential.reference)
            .await?
            .context("antigravity credential inspection missing")?;
        let view = inspection
            .representations
            .iter()
            .find(|r| {
                r.interface == "acp"
                    && r.generation == credential.generation
                    && r.current_generation
            })
            .context("antigravity acp representation missing")?;
        let representation = cred_store
            .representation(&view.id)
            .await?
            .context("antigravity acp representation entity missing")?;
        let locator = representation
            .secret_locator
            .context("missing secret locator for antigravity acp")?;
        let bundle = backend.read(locator).await?;
        let (token, settings) = decode_bundle(&bundle)?;

        let token_file = auth_store_dir.join("acp_token.json");
        tokio::fs::write(&token_file, token.expose()).await?;
        std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600))?;

        let settings_file = auth_store_dir.join("settings.json");
        tokio::fs::write(&settings_file, settings.expose()).await?;
        std::fs::set_permissions(&settings_file, std::fs::Permissions::from_mode(0o600))?;

        let binding_name = "antigravity-role-v1";
        let launch = Launch {
            adapter: Adapter::Antigravity,
            image: target
                .runtime_image_digest
                .clone()
                .unwrap_or_else(|| ANTIGRAVITY_IMAGE.into()),
            command: vec![ACP_EXECUTABLE.into()],
            agent_name: "antigravity-acp".into(),
            agent_version: "agy_acp_server_1.1.1".into(),
            binary_revision: "1.1.1".into(),
            cpu_millis: 1000,
            memory_mib: 512,
            network: AgentNetwork::Host,
        };
        let auth = Auth {
            source: "antigravity".into(),
            owner: credential.reference.clone(),
            account_class: "personal".into(),
            mode: AuthMode::LocalSession,
        };
        let descriptor = Descriptor {
            agent_id: "antigravity-acp".into(),
            agent_revision: "1.1.1".into(),
            launch_digest: launch.digest()?,
            protocol_version: 1,
            auth: auth.clone(),
            security_profile: SecurityProfile::Trusted,
            filesystem_policy: FilesystemPolicy::AttemptWorkspace,
            terminal_policy: TerminalPolicy::WorkspaceSupervisor,
            model_policy: ModelPolicy::Exact,
            accounting: Accounting::ExecutionOnly,
            max_limits: AcpLimits {
                prompt_turns: 1,
                broker_calls: 32,
                reported_tool_calls: 64,
                turn_timeout_seconds: 300,
                terminal_timeout_seconds: 30,
                terminal_runtime_seconds: 0,
                output_bytes: 524288,
            },
        };
        let mut files = BTreeMap::new();
        files.insert(
            "acp_token.json".into(),
            ".gemini/antigravity-acp/acp_token.json".into(),
        );
        files.insert(
            "settings.json".into(),
            ".gemini/antigravity-acp/settings.json".into(),
        );
        let rt = Runtime {
            binding_name: binding_name.into(),
            binding: Binding {
                model: target
                    .resolved_model
                    .clone()
                    .or_else(|| Some("gemini-3.8-flash".into())),
                runtime: "agent.antigravity-role-v1".into(),
                tools: Default::default(),
                permissions: Vec::new(),
                max_budget: Budget {
                    tokens: None,
                    cost_microusd: None,
                    calls: 1,
                },
                max_delegations: 0,
                acp: Some(descriptor),
            },
            launch,
            auth: AuthStore {
                path: auth_store_dir.clone(),
                source: auth.source,
                owner: auth.owner,
                account_class: auth.account_class,
                files,
                scopes: Vec::new(),
            },
            reasoning_effort: None,
        };
        rt.validate()?;
        rt
    } else {
        bail!("unsupported role provider: {}", target.provider);
    };

    let allowed_tools = if role.workspace_access == WorkspaceAccess::ReadOnly {
        vec!["read_file".to_string()]
    } else {
        vec![
            "read_file".to_string(),
            "write_file".to_string(),
            "create_directory".to_string(),
            "move".to_string(),
            "delete_file".to_string(),
            "delete_directory".to_string(),
        ]
    };

    let request_path = scratch_dir.path().join("request.json");
    let current_credential = cred_store
        .get(credential_ref)
        .await?
        .context("target credential disappeared before execution")?;
    validate_role_credential_target(target, &current_credential)?;
    let req = crate::acp_process::Request {
        runtime,
        attempt_id: id(),
        timeout_seconds: 600,
        tools: allowed_tools.clone(),
    };
    tokio::fs::write(&request_path, serde_json::to_vec(&req)?).await?;
    std::fs::set_permissions(&request_path, std::fs::Permissions::from_mode(0o600))?;

    let mut command = tokio::process::Command::new(crate::worker::current_executable()?);
    command
        .arg("acp-supervisor")
        .arg("--request")
        .arg(&request_path)
        .current_dir(scratch_dir.path())
        .env_clear()
        .env(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
        )
        .env(
            "HOME",
            std::env::var_os("HOME").context("rootless runtime HOME missing")?,
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    command.process_group(0);

    if let Some(val) = std::env::var_os("XDG_RUNTIME_DIR") {
        command.env("XDG_RUNTIME_DIR", val);
    }

    let mut child = command.spawn().context("failed to spawn acp-supervisor")?;

    let child_out = child
        .stdout
        .take()
        .ok_or_else(|| UnconfirmedRoleCleanup("supervisor stdout missing".into()))?;
    let child_in = child
        .stdin
        .take()
        .ok_or_else(|| UnconfirmedRoleCleanup("supervisor stdin missing".into()))?;
    let mut wire = Wire::new(child_out, child_in, 16 * 1024 * 1024);

    let mut state = AcpTurnState {
        repo_path,
        workspace_access: role.workspace_access,
        role_id: Some(role.role_id.clone()),
        workspace_identity: wf_run.repository_path.clone(),
        tool_call_limit: 64,
        agent_output: String::new(),
        tool_calls: 0,
        tool_successes: 0,
        tool_failures: 0,
        tool_counts: BTreeMap::new(),
        terminals: BTreeMap::new(),
        wf_attempt_id: Some(wf_run.attempt_id.clone()),
        role_exec_id: Some(role_exec.id.clone()),
        pool: Some(pool),
    };

    let turn = tokio::select! {
        result = async {
    let _init_res = acp_call(
        &mut wire,
        &mut state,
        "initialize",
        serde_json::json!({
            "protocolVersion": 1,
            "clientInfo": {
                "name": "orbit",
                "version": env!("CARGO_PKG_VERSION")
            },
            "clientCapabilities": {
                "fs": {
                    "readTextFile": true,
                    "writeTextFile": role.workspace_access == WorkspaceAccess::ReadWrite,
                    "listDirectory": true,
                    "findPath": true,
                    "editFile": role.workspace_access == WorkspaceAccess::ReadWrite,
                    "copy": role.workspace_access == WorkspaceAccess::ReadWrite,
                    "createDirectory": role.workspace_access == WorkspaceAccess::ReadWrite,
                    "move": role.workspace_access == WorkspaceAccess::ReadWrite,
                    "deleteFile": role.workspace_access == WorkspaceAccess::ReadWrite,
                    "deleteDirectory": role.workspace_access == WorkspaceAccess::ReadWrite
                },
                "search": {
                    "grep": true
                },
                "git": {
                    "status": true,
                    "diff": true,
                    "show": true
                },
                "terminal": false
            }
        }),
    )
    .await
    .context("ACP initialize failed")?;

    let new_res = acp_call(
        &mut wire,
        &mut state,
        "session/new",
        serde_json::json!({
            "cwd": crate::acp_runtime::WORKSPACE,
            "mcpServers": []
        }),
    )
    .await
    .context("ACP session/new failed")?;

    let session_id = new_res
        .get("sessionId")
        .and_then(|v| v.as_str())
        .context("sessionId missing in session/new response")?
        .to_string();

    if target.provider == "antigravity" {
        let _ = acp_call(
            &mut wire,
            &mut state,
            "session/set_mode",
            serde_json::json!({
                "sessionId": session_id,
                "modeId": "yolo"
            }),
        )
        .await;
    }

    let git_diff = if role.role_id == "reviewer" {
        Some(
            review_candidate_diff(repo_path, wf_run.base_revision.as_deref().unwrap_or("HEAD"))
                .await?,
        )
    } else {
        None
    };

    let base_rev = wf_run.base_revision.as_deref().unwrap_or("HEAD");
    let prompt_text = build_role_prompt(
        role,
        task_text,
        repo_path,
        base_rev,
        input_handoff,
        git_diff.as_deref(),
    );

    let prompt_res = acp_call(
        &mut wire,
        &mut state,
        "session/prompt",
        serde_json::json!({
            "sessionId": session_id,
            "prompt": [
                {
                    "type": "text",
                    "text": prompt_text
                }
            ]
        }),
    )
    .await
    .context("ACP session/prompt failed")?;

    if !state.agent_output.contains(ORBIT_HANDOFF_START) {
        extract_text_from_json(&prompt_res, &mut state.agent_output);
    }
    Ok::<(), anyhow::Error>(())
        } => result,
        _ = async {
            if *cancellation.borrow() { return; }
            while cancellation.changed().await.is_ok() {
                if *cancellation.borrow() { return; }
            }
            std::future::pending::<()>().await;
        } => Err(anyhow::anyhow!("ROLE_EXECUTION_CANCELLED")),
        _ = tokio::time::sleep(Duration::from_secs(600)) => Err(anyhow::anyhow!("ROLE_SUPERVISOR_TIMEOUT")),
    };

    drop(wire);
    let wait_limit = if turn.is_err() { 5 } else { 60 };
    let status = wait_cli_supervisor(
        &mut child,
        Duration::from_secs(wait_limit),
        Duration::from_secs(10),
    )
    .await?;
    let evidence = classify_supervisor_evidence(
        status,
        crate::acp_process::read_cleanup(&request_path, Some(&req.attempt_id)),
    );
    for (_tid, term) in std::mem::take(&mut state.terminals) {
        term.kill()
            .await
            .map_err(|error| UnconfirmedRoleCleanup(format!("terminal cleanup failed: {error}")))?;
    }

    let finished_at_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as i64;

    let store = WorkflowStore::new(pool.clone());

    let mut failure = turn.err();
    if let Err(error) = validate_role_turn_completion(&state.agent_output, &evidence) {
        failure.get_or_insert(error);
    }
    let status_text = if failure.is_some() {
        "FAILED"
    } else {
        "SUCCEEDED"
    };
    let reason = if failure.is_some() {
        "local execution unconfirmed"
    } else {
        "completed"
    };
    let failure_message = failure
        .as_ref()
        .map(|error| error.to_string().chars().take(512).collect::<String>());
    let model_evidence = role_model_evidence(target, None);
    store
        .insert_agent_execution(
            &agent_exec_id,
            &role_exec.id,
            &format!("{}-acp", target.provider),
            Some(&target.provider),
            target.resolved_model.as_deref(),
            started_at_ms,
            Some(finished_at_ms),
            status_text,
            Some(reason),
            evidence.exit_code,
            failure_message.as_deref(),
            model_evidence.requested,
            model_evidence.configured,
            model_evidence.observed,
            1,
            state.tool_calls as i64,
            state.tool_successes as i64,
            state.tool_failures as i64,
            &serde_json::to_value(&state.tool_counts)?,
            &serde_json::json!({ "provider": target.provider, "cleanup_confirmed": evidence.cleanup_confirmed, "observed_model": null }),
        )
        .await?;
    store
        .record_agent_execution(&role_exec.id, &agent_exec_id)
        .await?;

    if !evidence.cleanup_confirmed {
        return Err(UnconfirmedRoleCleanup(
            failure_message.unwrap_or_else(|| "missing matching cleanup receipt".into()),
        )
        .into());
    }

    if let Some(error) = failure {
        return Err(error);
    }

    Ok(RoleExecutionOutcome {
        raw_output: state.agent_output,
        agent_execution_ids: vec![agent_exec_id],
        termination_reason: Some("completed".into()),
    })
}

/// Production ACP role agent executor that executes real ACP agent turns.
pub struct RealAcpRoleExecutor;

#[async_trait::async_trait]
impl RoleAgentExecutor for RealAcpRoleExecutor {
    async fn execute_role(
        &self,
        pool: &PgPool,
        wf_run: &WorkflowRun,
        role_exec: &RoleExecution,
        role: &RoleDefinition,
        target: &ResolvedExecutionTarget,
        task_text: &str,
        repo_path: &Path,
        input_handoff: Option<&HandoffArtifact>,
        cancellation: tokio::sync::watch::Receiver<bool>,
    ) -> Result<RoleExecutionOutcome> {
        // Enforce role permissions and capabilities:
        // Planner & Reviewer: read-only, only read_file tool.
        // Implementer: read-write, full filesystem tools.
        let _allowed_tools = if role.workspace_access == WorkspaceAccess::ReadOnly {
            vec!["read_file".to_string()]
        } else {
            vec![
                "read_file".to_string(),
                "write_file".to_string(),
                "create_directory".to_string(),
                "move".to_string(),
                "delete_file".to_string(),
                "delete_directory".to_string(),
            ]
        };

        execute_real_acp_turn(
            pool,
            wf_run,
            role_exec,
            role,
            target,
            task_text,
            repo_path,
            input_handoff,
            cancellation,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn git_fixture(repo: &Path, args: &[&str]) -> Result<String> {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()?;
        ensure!(
            output.status.success(),
            "git fixture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }

    #[tokio::test]
    async fn candidate_v2_accounts_for_tracked_and_untracked_changes() -> Result<()> {
        let repo = tempdir()?;
        git_fixture(repo.path(), &["init", "-q"])?;
        git_fixture(
            repo.path(),
            &["config", "user.email", "orbit@example.invalid"],
        )?;
        git_fixture(repo.path(), &["config", "user.name", "Orbit fixture"])?;
        std::fs::write(repo.path().join("tracked.txt"), b"original")?;
        git_fixture(repo.path(), &["add", "tracked.txt"])?;
        git_fixture(repo.path(), &["commit", "-qm", "base"])?;
        let baseline = git_fixture(repo.path(), &["rev-parse", "HEAD"])?;
        let clean = compute_workspace_state(repo.path(), &baseline).await?;
        assert_eq!(clean.digest_version, 2);

        std::fs::write(repo.path().join("new.txt"), b"first")?;
        let untracked = compute_workspace_state(repo.path(), &baseline).await?;
        assert_ne!(clean.state_id, untracked.state_id);
        std::fs::write(repo.path().join("new.txt"), b"second")?;
        let changed_untracked = compute_workspace_state(repo.path(), &baseline).await?;
        assert_ne!(untracked.state_id, changed_untracked.state_id);

        git_fixture(repo.path(), &["add", "new.txt"])?;
        let staged_addition = compute_workspace_state(repo.path(), &baseline).await?;
        assert_ne!(staged_addition.state_id, clean.state_id);
        std::fs::remove_file(repo.path().join("tracked.txt"))?;
        let deletion = compute_workspace_state(repo.path(), &baseline).await?;
        assert_ne!(staged_addition.state_id, deletion.state_id);
        Ok(())
    }

    #[tokio::test]
    async fn git_failure_and_non_git_read_failure_cannot_look_clean() -> Result<()> {
        let bad_git = tempdir()?;
        std::fs::create_dir(bad_git.path().join(".git"))?;
        assert!(
            compute_workspace_state(bad_git.path(), "HEAD")
                .await
                .is_err()
        );

        let plain = tempdir()?;
        std::fs::create_dir(plain.path().join("nested"))?;
        std::fs::write(plain.path().join("nested/file.txt"), b"one")?;
        let first = compute_workspace_state(plain.path(), "base").await?;
        std::fs::write(plain.path().join("nested/file.txt"), b"two")?;
        let second = compute_workspace_state(plain.path(), "base").await?;
        assert_ne!(first.state_id, second.state_id);
        assert!(hash_candidate(plain.path(), &[], &[PathBuf::from("missing.txt")]).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn reviewer_diff_contains_untracked_bytes_and_reports_git_failure() -> Result<()> {
        let repo = tempdir()?;
        git_fixture(repo.path(), &["init", "-q"])?;
        git_fixture(
            repo.path(),
            &["config", "user.email", "orbit@example.invalid"],
        )?;
        git_fixture(repo.path(), &["config", "user.name", "Orbit fixture"])?;
        std::fs::write(repo.path().join("tracked.txt"), b"base")?;
        git_fixture(repo.path(), &["add", "tracked.txt"])?;
        git_fixture(repo.path(), &["commit", "-qm", "base"])?;
        let baseline = git_fixture(repo.path(), &["rev-parse", "HEAD"])?;
        std::fs::write(repo.path().join("untracked.txt"), b"review these bytes")?;
        let diff = review_candidate_diff(repo.path(), &baseline).await?;
        assert!(diff.contains("untracked.txt"));
        assert!(diff.contains("review these bytes"));
        assert!(
            review_candidate_diff(repo.path(), "missing-revision")
                .await
                .is_err()
        );
        Ok(())
    }

    fn make_test_wire() -> (Wire, Wire) {
        let (client_r, server_w) = tokio::io::duplex(65536);
        let (server_r, client_w) = tokio::io::duplex(65536);
        let server_wire = Wire::new(server_r, server_w, 65536);
        let client_wire = Wire::new(client_r, client_w, 65536);
        (server_wire, client_wire)
    }

    #[tokio::test]
    async fn test_handle_acp_message_mutation_requires_lock_context() -> Result<()> {
        let repo = tempdir()?;
        let repo_path = repo.path();
        let (mut server_wire, mut client_wire) = make_test_wire();

        let mut state = AcpTurnState::new(repo_path, WorkspaceAccess::ReadWrite);
        state.role_id = Some("implementer".into());
        state.workspace_identity = Some(repo_path.canonicalize()?.to_string_lossy().into_owned());

        let msg = serde_json::json!({
            "id": 1,
            "method": "fs/create_directory",
            "params": {
                "path": "docs/archive",
                "recursive": true
            }
        });
        handle_acp_message(&mut server_wire, &mut state, msg).await?;
        let resp = client_wire.read().await?;
        assert_eq!(resp["id"], 1);
        assert!(
            resp["error"]["message"]
                .as_str()
                .unwrap()
                .contains(crate::tool_surface::ERR_MUTATION_LOCK_REQUIRED)
        );
        assert!(!repo_path.join("docs/archive").exists());
        assert_eq!(state.tool_calls, 1);
        assert_eq!(state.tool_successes, 0);
        assert_eq!(state.tool_failures, 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_handle_acp_message_denies_unconfined_terminal_creation() -> Result<()> {
        let repo = tempdir()?;
        let repo_path = repo.path();
        let (mut server_wire, mut client_wire) = make_test_wire();
        let mut state = AcpTurnState::new(repo_path, WorkspaceAccess::ReadWrite);

        handle_acp_message(
            &mut server_wire,
            &mut state,
            serde_json::json!({
                "id": 1,
                "method": "terminal/create",
                "params": {"command":"sh", "args":["-c", "touch should-not-exist"]}
            }),
        )
        .await?;

        let response = client_wire.read().await?;
        assert!(response.get("error").is_some());
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains(ERR_CLI_WORKFLOW_TERMINAL_DISABLED)
        );
        assert!(!repo_path.join("should-not-exist").exists());
        assert!(state.terminals.is_empty());
        assert_eq!(state.tool_failures, 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_handle_acp_message_read_only_denial() -> Result<()> {
        let repo = tempdir()?;
        let repo_path = repo.path();
        let (mut server_wire, mut client_wire) = make_test_wire();

        let mut state = AcpTurnState::new(repo_path, WorkspaceAccess::ReadOnly);

        // Try mutation under ReadOnly
        let methods = [
            "fs/create_directory",
            "fs/move",
            "fs/delete_file",
            "fs/delete_directory",
            "fs/write_text_file",
        ];
        for (i, method) in methods.iter().enumerate() {
            let msg = serde_json::json!({
                "id": i + 1,
                "method": method,
                "params": {
                    "path": "docs/test.md"
                }
            });
            handle_acp_message(&mut server_wire, &mut state, msg).await?;
            let resp = client_wire.read().await?;
            assert!(resp.get("error").is_some());
            assert!(
                resp["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("read-only")
            );
        }

        assert_eq!(state.tool_calls, 5);
        assert_eq!(state.tool_failures, 5);
        assert_eq!(state.tool_successes, 0);

        Ok(())
    }

    #[test]
    fn workflow_verification_has_no_weak_authoritative_fallbacks() {
        let source = include_str!("workflow_coordinator.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(!source.contains("tier-cargo-plan"));
        assert!(!source.contains("tier-docs-plan"));
        assert!(!source.contains("unwrap_or_else(|| vec![\"true\".into()])"));
    }

    #[test]
    fn s7_handoff_needs_successful_supervisor_and_matching_cleanup() {
        use std::os::unix::process::ExitStatusExt;
        let handoff = format!("{ORBIT_HANDOFF_START}\n{{}}\n{ORBIT_HANDOFF_END}");
        let nonzero =
            classify_supervisor_evidence(std::process::ExitStatus::from_raw(7 << 8), Ok(7));
        assert!(nonzero.cleanup_confirmed);
        assert_eq!(nonzero.exit_code, Some(7));
        assert!(
            validate_role_turn_completion(&handoff, &nonzero)
                .unwrap_err()
                .to_string()
                .contains("ROLE_SUPERVISOR_EXIT_FAILED")
        );

        let signal = classify_supervisor_evidence(
            std::process::ExitStatus::from_raw(libc::SIGTERM),
            Ok(128 + libc::SIGTERM),
        );
        assert_eq!(signal.exit_code, Some(128 + libc::SIGTERM));
        assert!(validate_role_turn_completion(&handoff, &signal).is_err());

        let missing = classify_supervisor_evidence(
            std::process::ExitStatus::from_raw(0),
            Err(anyhow::anyhow!("missing receipt")),
        );
        assert!(!missing.cleanup_confirmed);
        assert!(
            validate_role_turn_completion(&handoff, &missing)
                .unwrap_err()
                .to_string()
                .contains("ROLE_CLEANUP_UNCONFIRMED")
        );

        let successful = classify_supervisor_evidence(std::process::ExitStatus::from_raw(0), Ok(0));
        assert!(validate_role_turn_completion(&handoff, &successful).is_ok());
    }

    #[tokio::test]
    async fn s7_supervisor_wait_is_bounded_and_kills_process_group() -> Result<()> {
        let mut command = tokio::process::Command::new("sh");
        command
            .args(["-c", "sleep 30"])
            .process_group(0)
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let started = tokio::time::Instant::now();
        let status = wait_cli_supervisor(
            &mut child,
            Duration::from_millis(50),
            Duration::from_secs(2),
        )
        .await?;
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(!status.success());
        Ok(())
    }

    #[test]
    fn s7_wrong_credential_generation_is_rejected() {
        let target = ResolvedExecutionTarget {
            provider: "codex".into(),
            runtime_interface: "codex-acp".into(),
            credential_id: Some("codex-main".into()),
            credential_generation: Some(3),
            requested_model: Some("requested".into()),
            resolved_model: Some("configured".into()),
            runtime_image_digest: None,
            resolution_reason: "test".into(),
        };
        let credential = crate::credential_registry::Credential {
            id: "fixture".into(),
            provider: "codex".into(),
            reference: "codex-main".into(),
            generation: 4,
            endpoint: None,
            auth_type: "local-session".into(),
            secret_backend: "local-private".into(),
            secret_locator: None,
            status: crate::credential_registry::CredentialStatus::Enrolled,
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        let provenance = role_model_evidence(&target, None);
        assert_eq!(provenance.requested, Some("requested"));
        assert_eq!(provenance.configured, Some("configured"));
        assert_eq!(provenance.observed, None);
        assert!(
            validate_role_credential_target(&target, &credential)
                .unwrap_err()
                .to_string()
                .contains("CREDENTIAL_GENERATION_CHANGED")
        );
        let same = crate::credential_registry::Credential {
            generation: 3,
            ..credential
        };
        assert!(validate_role_credential_target(&target, &same).is_ok());
    }
}
