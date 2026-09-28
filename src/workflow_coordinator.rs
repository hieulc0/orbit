//! Production workflow stage progression, role execution, and evidence coordination.
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
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
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

                // Repository mutations require a matching persisted execution lock owner.
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

fn repository_tools_for_role(role: &RoleDefinition) -> Vec<String> {
    use crate::tool_surface::{CanonicalToolName as Tool, ToolMetadata};

    const REPOSITORY_TOOLS: [Tool; 14] = [
        Tool::FsReadTextFile,
        Tool::FsListDirectory,
        Tool::FsFindPath,
        Tool::SearchGrep,
        Tool::GitStatus,
        Tool::GitDiff,
        Tool::GitShow,
        Tool::FsWriteTextFile,
        Tool::FsEditFile,
        Tool::FsCreateDirectory,
        Tool::FsMove,
        Tool::FsCopy,
        Tool::FsDeleteFile,
        Tool::FsDeleteDirectory,
    ];

    REPOSITORY_TOOLS
        .into_iter()
        .filter_map(|tool| {
            let metadata = ToolMetadata::for_tool(tool);
            (metadata.is_role_allowed(&role.role_id)
                && (!metadata.mutating || role.workspace_access == WorkspaceAccess::ReadWrite))
                .then(|| tool.legacy_name().to_owned())
        })
        .collect()
}

fn build_role_prompt(
    role: &RoleDefinition,
    task_text: &str,
    repo_path: &Path,
    base_revision: &str,
    input_handoff: Option<&HandoffArtifact>,
    git_diff: Option<&str>,
) -> String {
    let orbit_acp_tool_names = repository_tools_for_role(role).join(", ");
    let workspace_path = crate::acp_runtime::WORKSPACE;
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
            "You are the PLANNER role in an Orbit automated software change workflow.\n            Your responsibility is to analyze the task, inspect the repository using read-only tools, and produce a clear, structured implementation plan.\n\n            TASK OBJECTIVE:\n{task_text}\n\n            REPOSITORY CONTEXT:\n            Repository Workspace: {workspace_path}\n            Repository tool paths are relative to this workspace.\n            Base Revision: {base_revision}{docs_manifest}\n            WORKSPACE PERMISSIONS:\n            You have READ-ONLY workspace access. You can inspect the repository using:\n            - fs/read_text_file (or read_file): read file content\n            - fs/list_directory: inspect workspace directory entries\n            - fs/find_path: search for files matching patterns\n            - search/grep: regex or text search across files\n            - git/status, git/diff, git/show: inspect git working tree and commit history\n            You CANNOT write or edit files, and CANNOT create terminals.\n\n            INSTRUCTIONS:\n            1. Inspect existing files, search patterns, and repository structure using the read-only tools.\n            2. Formulate a concrete step-by-step implementation plan.\n            3. You MUST end your response with a structured JSON plan handoff block inside the exact delimiters:\n            <<<ORBIT_HANDOFF_START>>>\n            {{\n              \"summary\": \"Concise summary of the plan\",\n              \"affected_areas\": [\"area1\", \"area2\"],\n              \"implementation_steps\": [\"step 1\", \"step 2\"],\n              \"expected_files\": [\"docs/file1.md\"],\n              \"risks\": [],\n              \"verification_notes\": [\"verification instructions\"],\n              \"open_questions\": []\n            }}\n            <<<ORBIT_HANDOFF_END>>>\n",
            workspace_path = workspace_path,
            base_revision = base_revision,
            task_text = task_text,
            docs_manifest = docs_manifest,
        ),
        "implementer" => {
            let plan_summary = input_handoff
                .map(|h| h.structured_payload.to_string())
                .unwrap_or_else(|| "No prior plan provided.".to_string());
            format!(
                "You are the IMPLEMENTER role in an Orbit automated software change workflow.\n                Your responsibility is to execute the implementation plan by modifying project files and verifying your work.\n\n                TASK OBJECTIVE:\n{task_text}\n\n                PLANNER SPECIFICATION:\n{plan_summary}\n\n                REPOSITORY CONTEXT:\n                Repository Workspace: {workspace_path}\n                Repository tool paths are relative to this workspace.\n                Base Revision: {base_revision}{docs_manifest}\n                WORKSPACE PERMISSIONS:\n                You have FULL READ-WRITE coding agent workspace access. Tools available to you:\n                - fs/read_text_file: read file contents\n                - fs/write_text_file: write complete file contents\n                - fs/edit_file: perform targeted text replacements (old_text -> new_text, replace_all)\n                - fs/list_directory: list directory contents\n                - fs/find_path: search workspace file paths by pattern\n                - fs/create_directory: create a new directory\n                - fs/move: move or rename files/directories\n                - fs/copy: copy files or directories\n                - fs/delete_file: remove a single file\n                - fs/delete_directory: remove a directory\n                - search/grep: ripgrep workspace code\n                - git/status, git/diff, git/show: inspect git status, diffs, and commits\n                - terminal/create, terminal/output, terminal/wait_for_exit, terminal/kill, terminal/release: run tests or commands\n\n                INSTRUCTIONS:\n                1. Implement all required changes and directory reorganization per the planner specification.\n                2. Use fs/edit_file for surgical modifications and fs/write_text_file for new files.\n                3. You MUST end your response with a structured JSON implementation handoff block inside the exact delimiters:\n                <<<ORBIT_HANDOFF_START>>>\n                {{\n                  \"summary\": \"Concise summary of changes implemented\",\n                  \"changed_files\": [\"docs/file1.md\"],\n                  \"tests_added_or_modified\": [],\n                  \"exploratory_commands\": [],\n                  \"known_limitations\": [],\n                  \"verification_notes\": [\"self-verification details\"]\n                }}\n                <<<ORBIT_HANDOFF_END>>>\n",
                workspace_path = workspace_path,
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
    let prompt = format!("{prompt}\n\nORBIT/ACP TOOL NAMES: {orbit_acp_tool_names}");
    let prompt = if role.role_id == "implementer" {
        format!(
            "{prompt}\n\nDiscover paths with list_directory, find_path, or grep before guessing names for files the task does not identify. If a lookup returns PATH_NOT_FOUND, inspect the workspace with those tools and retry using a discovered path."
        )
    } else {
        prompt
    };
    let prompt = prompt.replace(
        "You can inspect files using fs/read_text_file.",
        "You can inspect files using the listed read-only repository tools.",
    );
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
    tool_call_audit: ToolCallAudit,
    pub terminals: BTreeMap<String, std::sync::Arc<crate::tool_surface::AgentTerminal>>,
    pub wf_attempt_id: Option<String>,
    pub role_exec_id: Option<String>,
    pub agent_exec_id: Option<String>,
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
            tool_call_audit: ToolCallAudit::default(),
            terminals: BTreeMap::new(),
            wf_attempt_id: None,
            role_exec_id: None,
            agent_exec_id: None,
            pool: None,
        }
    }
}

const TOOL_CALL_AUDIT_LIMIT: usize = 64;
const PROVIDER_TOOL_NAME_QUEUE_LIMIT: usize = 64;
const TOOL_PATH_INPUT_LIMIT: usize = 4096;
const TOOL_PATH_DISPLAY_LIMIT: usize = 192;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum PathAuditState {
    WorkspaceRelative,
    OutsideWorkspaceRedacted,
    InvalidPath,
}

#[derive(Clone, Debug, serde::Serialize)]
struct PathArgumentAudit {
    argument: &'static str,
    state: PathAuditState,
    workspace_relative_path: Option<String>,
    exists: Option<bool>,
    display_truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum ToolCallOutcome {
    Success,
    ExpectedDenial,
    InvalidRequest,
    ExecutionFailure,
    Timeout,
    Cancelled,
    Unsupported,
}

#[derive(Clone, Debug, serde::Serialize)]
struct ToolCallAuditEntry {
    sequence: u64,
    tool_invocation_id: Option<String>,
    provider_tool_call_id: Option<String>,
    callback_request_id: Option<String>,
    callback_request_id_shape: &'static str,
    provider_tool_name: &'static str,
    provider_name_mapping: &'static str,
    provider_update_correlation: &'static str,
    provider_update_title_class: Option<&'static str>,
    provider_update_tool_kind: Option<&'static str>,
    provider_update_status: Option<&'static str>,
    provider_tool_call_id_shape: Option<&'static str>,
    canonical_tool_name: &'static str,
    advertised_to_provider: Option<bool>,
    role_allowed: Option<bool>,
    outcome: Option<ToolCallOutcome>,
    terminal_state: &'static str,
    error_code: Option<&'static str>,
    mutating: Option<bool>,
    mutation_applied: Option<bool>,
    later_callback_observed: bool,
    turn_completed: Option<bool>,
    path_arguments: Vec<PathArgumentAudit>,
}

#[derive(Clone, Copy, Debug)]
struct ProviderToolMethodObservation {
    provider_tool_name: &'static str,
    request_tool_name: &'static str,
    canonical_tool_name: crate::tool_surface::CanonicalToolName,
}

#[derive(Clone, Debug, serde::Serialize)]
struct ProviderToolUpdateObservation {
    tool_invocation_id: Option<String>,
    provider_tool_call_id: Option<String>,
    invocation_id_shape: &'static str,
    title_class: &'static str,
    tool_kind: &'static str,
    status: &'static str,
    tool_call_id_shape: &'static str,
    correlation_state: &'static str,
}

#[derive(Clone, Debug)]
struct ProviderToolInvocation {
    observation: ProviderToolUpdateObservation,
    callback_count: u64,
    invalidated: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct ActiveToolCall {
    sequence: u64,
    successes_before: u64,
    failures_before: u64,
    operation_error_code: Option<&'static str>,
}

#[derive(Default)]
struct ToolCallAudit {
    entries: Vec<ToolCallAuditEntry>,
    omitted_count: u64,
    unmatched_provider_call_count: u64,
    mutating_count: u64,
    mutating_unknown_count: u64,
    denied_count: u64,
    role_id: Option<String>,
    advertised_tools: Option<BTreeSet<String>>,
    provider_tool_invocations: BTreeMap<String, ProviderToolInvocation>,
    provider_tool_call_ids: HashMap<String, String>,
    seen_provider_tool_call_ids: HashSet<String>,
    unmatched_provider_updates: Vec<ProviderToolUpdateObservation>,
    callback_request_ids: HashSet<String>,
    correlation_supported: bool,
    correlation_partial: bool,
    provider_tool_names_omitted: u64,
    unmatched_callback_count: u64,
    active_call: Option<ActiveToolCall>,
}

impl ToolCallAudit {
    fn with_context(role_id: Option<&str>, advertised_tools: Option<&[String]>) -> Self {
        let mut audit = Self::default();
        audit.set_context(role_id, advertised_tools);
        audit
    }

    fn set_context(&mut self, role_id: Option<&str>, advertised_tools: Option<&[String]>) {
        self.role_id = role_id.map(str::to_owned);
        self.advertised_tools = advertised_tools.map(|tools| tools.iter().cloned().collect());
    }

    fn set_role_id(&mut self, role_id: Option<&str>) {
        self.role_id = role_id.map(str::to_owned);
    }

    #[cfg(test)]
    fn observe_provider_tool_name(&mut self, update: &serde_json::Value) {
        self.observe_provider_tool_name_with_metadata(update, Ok(None));
    }

    fn observe_provider_tool_name_with_metadata(
        &mut self,
        update: &serde_json::Value,
        invocation_metadata: Result<Option<crate::acp_wire::OrbitToolInvocationMeta>>,
    ) {
        if update
            .get("sessionUpdate")
            .and_then(serde_json::Value::as_str)
            != Some("tool_call")
        {
            return;
        }

        if self.provider_tool_invocations.len() + self.unmatched_provider_updates.len()
            >= PROVIDER_TOOL_NAME_QUEUE_LIMIT
        {
            self.provider_tool_names_omitted = self.provider_tool_names_omitted.saturating_add(1);
            self.correlation_partial = true;
            return;
        }

        let (tool_invocation_id, provider_tool_call_id, invocation_id_shape) =
            match invocation_metadata {
                Ok(Some(meta)) => (
                    Some(meta.invocation_id),
                    Some(meta.provider_tool_call_id),
                    "valid",
                ),
                Ok(None) => (None, None, "missing"),
                Err(_) => (None, None, "malformed"),
            };

        let title = update.get("title");
        let title_class = match title {
            Some(serde_json::Value::String(value)) if value.is_empty() => "empty_string",
            Some(serde_json::Value::String(_)) => "non_empty_string",
            Some(_) => "non_string",
            None => "missing",
        };
        let tool_kind = classify_provider_update_value(
            update.get("kind"),
            &[
                "read",
                "edit",
                "delete",
                "move",
                "search",
                "execute",
                "think",
                "fetch",
                "switch_mode",
                "other",
            ],
        );
        let status = classify_provider_update_value(
            update.get("status"),
            &[
                "pending",
                "in_progress",
                "completed",
                "failed",
                "cancelled",
                "canceled",
            ],
        );
        let (observed_provider_id, mut tool_call_id_shape) = match update.get("toolCallId") {
            Some(serde_json::Value::String(id))
                if !id.is_empty() && id.len() <= 256 && safe_correlator_id(id, 256) =>
            {
                (Some(id.clone()), "string")
            }
            Some(serde_json::Value::String(id)) if id.is_empty() => (None, "empty_string"),
            Some(serde_json::Value::String(id)) if id.len() > 256 => (None, "oversized_string"),
            Some(serde_json::Value::String(_)) => (None, "unsafe_string"),
            Some(_) => (None, "non_string"),
            None => (None, "missing"),
        };
        let duplicate_provider_id = observed_provider_id
            .as_ref()
            .is_some_and(|id| !self.seen_provider_tool_call_ids.insert(id.clone()));
        if duplicate_provider_id {
            tool_call_id_shape = "duplicate_string";
        }

        let mut correlation_state = "UNRESOLVED";
        if let (Some(invocation_id), Some(provider_call_id), Some(observed_id)) = (
            tool_invocation_id.as_deref(),
            provider_tool_call_id.as_deref(),
            observed_provider_id.as_deref(),
        ) {
            if provider_call_id != observed_id {
                correlation_state = "PROVIDER_ID_MISMATCH";
            } else if duplicate_provider_id {
                correlation_state = "DUPLICATE_PROVIDER_ID";
                if let Some(previous_invocation) = self.provider_tool_call_ids.get(provider_call_id)
                    && let Some(previous) =
                        self.provider_tool_invocations.get_mut(previous_invocation)
                {
                    previous.invalidated = true;
                    previous.observation.correlation_state = "DUPLICATE_PROVIDER_ID";
                }
            } else if let Some(previous_invocation) =
                self.provider_tool_call_ids.get(provider_call_id)
            {
                correlation_state = "DUPLICATE_PROVIDER_ID";
                if let Some(previous) = self.provider_tool_invocations.get_mut(previous_invocation)
                {
                    previous.invalidated = true;
                    previous.observation.correlation_state = "DUPLICATE_PROVIDER_ID";
                }
            } else if self.provider_tool_invocations.contains_key(invocation_id) {
                correlation_state = "DUPLICATE_INVOCATION_ID";
                if let Some(previous) = self.provider_tool_invocations.get_mut(invocation_id) {
                    previous.invalidated = true;
                    previous.observation.correlation_state = "DUPLICATE_INVOCATION_ID";
                }
            } else {
                correlation_state = "OBSERVED";
                let observation = ProviderToolUpdateObservation {
                    tool_invocation_id: tool_invocation_id.clone(),
                    provider_tool_call_id: observed_provider_id.clone(),
                    invocation_id_shape,
                    title_class,
                    tool_kind,
                    status,
                    tool_call_id_shape,
                    correlation_state,
                };
                self.provider_tool_call_ids
                    .insert(provider_call_id.to_owned(), invocation_id.to_owned());
                self.provider_tool_invocations.insert(
                    invocation_id.to_owned(),
                    ProviderToolInvocation {
                        observation,
                        callback_count: 0,
                        invalidated: false,
                    },
                );
                return;
            }
        }

        self.correlation_partial = true;
        self.unmatched_provider_updates
            .push(ProviderToolUpdateObservation {
                tool_invocation_id,
                provider_tool_call_id: observed_provider_id,
                invocation_id_shape,
                title_class,
                tool_kind,
                status,
                tool_call_id_shape,
                correlation_state,
            });
    }

    #[cfg(test)]
    fn begin_call(
        &mut self,
        sequence: u64,
        provider_method: &str,
        canonical_tool: Option<crate::tool_surface::CanonicalToolName>,
        successes_before: u64,
        failures_before: u64,
    ) -> u64 {
        self.begin_call_with_context(
            sequence,
            provider_method,
            None,
            Ok(None),
            canonical_tool,
            successes_before,
            failures_before,
        )
    }

    fn begin_call_with_context(
        &mut self,
        sequence: u64,
        provider_method: &str,
        request_id: Option<&serde_json::Value>,
        invocation_metadata: Result<Option<crate::acp_wire::OrbitToolInvocationMeta>>,
        canonical_tool: Option<crate::tool_surface::CanonicalToolName>,
        successes_before: u64,
        failures_before: u64,
    ) -> u64 {
        for entry in &mut self.entries {
            entry.later_callback_observed = true;
        }

        let callback_request_id = request_id.and_then(safe_json_rpc_request_id);
        let callback_request_id_shape = if request_id.is_none() {
            "missing"
        } else if callback_request_id.is_some() {
            "valid"
        } else {
            "invalid"
        };
        let mut duplicate_callback = false;
        let mut callback_tracking_overflow = false;
        if let Some(id) = callback_request_id.as_ref() {
            if self.callback_request_ids.contains(id) {
                duplicate_callback = true;
            } else if self.callback_request_ids.len() >= TOOL_CALL_AUDIT_LIMIT {
                callback_tracking_overflow = true;
            } else {
                self.callback_request_ids.insert(id.clone());
            }
        }
        let (invocation_metadata, metadata_shape) = match invocation_metadata {
            Ok(Some(meta)) => (Some(meta), "valid"),
            Ok(None) => (None, "missing"),
            Err(_) => (None, "malformed"),
        };
        let mut matched_update = None;
        let mut provider_update_correlation = match metadata_shape {
            "malformed" => "MALFORMED",
            "missing" => "CALLBACK_WITHOUT_INVOCATION_ID",
            _ if callback_tracking_overflow => "CALLBACK_TRACKING_LIMIT_EXCEEDED",
            _ if duplicate_callback => "DUPLICATE_CALLBACK_ID",
            _ if callback_request_id.is_none() => "INVALID_CALLBACK_ID",
            _ => "CALLBACK_WITHOUT_UPDATE",
        };
        if let Some(meta) = invocation_metadata.as_ref()
            && !duplicate_callback
            && !callback_tracking_overflow
            && callback_request_id.is_some()
        {
            if let Some(invocation) = self.provider_tool_invocations.get_mut(&meta.invocation_id) {
                if invocation.invalidated {
                    provider_update_correlation = "INVALIDATED_INVOCATION";
                } else if invocation.observation.provider_tool_call_id.as_deref()
                    != Some(meta.provider_tool_call_id.as_str())
                {
                    provider_update_correlation = "PROVIDER_ID_MISMATCH";
                } else if invocation.callback_count > 0 {
                    provider_update_correlation = "DUPLICATE_INVOCATION_CALLBACK";
                } else {
                    invocation.callback_count = 1;
                    matched_update = Some(invocation.observation.clone());
                    provider_update_correlation = "CORRELATED";
                    self.correlation_supported = true;
                }
            }
        }
        if provider_update_correlation != "CORRELATED" {
            self.correlation_partial = true;
            self.unmatched_callback_count = self.unmatched_callback_count.saturating_add(1);
        }
        let method_observation = provider_tool_method_observation(provider_method);
        let canonical_name = canonical_tool
            .map(|tool| tool.as_str())
            .unwrap_or("unknown");
        let (provider_name, provider_name_mapping) = match method_observation {
            Some(observation) => {
                let mapping = if Some(observation.canonical_tool_name) == canonical_tool {
                    "MATCH"
                } else {
                    "MISMATCH"
                };
                (observation.provider_tool_name, mapping)
            }
            None if canonical_tool.is_some() => ("unknown", "MISMATCH"),
            None => ("unknown", "UNKNOWN"),
        };
        let metadata = canonical_tool.map(crate::tool_surface::ToolMetadata::for_tool);
        let advertised_to_provider = method_observation.and_then(|observation| {
            self.advertised_tools
                .as_ref()
                .map(|tools| tools.contains(observation.request_tool_name))
        });
        let role_allowed = metadata.as_ref().and_then(|metadata| {
            self.role_id
                .as_deref()
                .map(|role_id| metadata.is_role_allowed(role_id))
        });
        let mutating = metadata.as_ref().map(|metadata| metadata.mutating);
        self.record_mutating(mutating);

        self.active_call = Some(ActiveToolCall {
            sequence,
            successes_before,
            failures_before,
            operation_error_code: None,
        });

        if self.entries.len() >= TOOL_CALL_AUDIT_LIMIT {
            self.omitted_count = self.omitted_count.saturating_add(1);
            return sequence;
        }

        self.entries.push(ToolCallAuditEntry {
            sequence,
            tool_invocation_id: invocation_metadata
                .as_ref()
                .map(|meta| meta.invocation_id.clone()),
            provider_tool_call_id: invocation_metadata
                .as_ref()
                .map(|meta| meta.provider_tool_call_id.clone()),
            callback_request_id: callback_request_id.clone(),
            callback_request_id_shape,
            provider_tool_name: provider_name,
            provider_name_mapping,
            provider_update_correlation,
            provider_update_title_class: Some(
                matched_update
                    .as_ref()
                    .map_or("update_not_observed", |update| update.title_class),
            ),
            provider_update_tool_kind: Some(
                matched_update
                    .as_ref()
                    .map_or("update_not_observed", |update| update.tool_kind),
            ),
            provider_update_status: Some(
                matched_update
                    .as_ref()
                    .map_or("update_not_observed", |update| update.status),
            ),
            provider_tool_call_id_shape: Some(
                matched_update
                    .as_ref()
                    .map_or("update_not_observed", |update| update.tool_call_id_shape),
            ),
            canonical_tool_name: canonical_name,
            advertised_to_provider,
            role_allowed,
            outcome: None,
            terminal_state: "DISPATCHED",
            error_code: None,
            mutating,
            mutation_applied: None,
            later_callback_observed: false,
            turn_completed: None,
            path_arguments: Vec::new(),
        });
        sequence
    }

    fn record_path_arguments(
        &mut self,
        sequence: u64,
        tool: crate::tool_surface::CanonicalToolName,
        params: &serde_json::Value,
        repo_path: &Path,
    ) {
        let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.sequence == sequence)
        else {
            return;
        };
        entry.path_arguments = path_arguments_for_tool(tool, params, repo_path);
    }

    fn record_operation_error(&mut self, sequence: u64, error: &anyhow::Error) {
        if let Some(active) = self
            .active_call
            .as_mut()
            .filter(|active| active.sequence == sequence)
        {
            active.operation_error_code = Some(normalized_tool_error_code(error));
        }
    }

    fn record_mutating(&mut self, mutating: Option<bool>) {
        if mutating == Some(true) {
            self.mutating_count = self.mutating_count.saturating_add(1);
        } else if mutating.is_none() {
            self.mutating_unknown_count = self.mutating_unknown_count.saturating_add(1);
        }
    }

    fn finish_call(
        &mut self,
        sequence: u64,
        outcome: ToolCallOutcome,
        error_code: Option<&'static str>,
    ) {
        let active = self
            .active_call
            .filter(|active| active.sequence == sequence);
        let error_code = error_code.or(active.and_then(|active| active.operation_error_code));
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.sequence == sequence)
        {
            entry.outcome = Some(outcome);
            entry.terminal_state = match outcome {
                ToolCallOutcome::Success => "SUCCESS",
                ToolCallOutcome::ExpectedDenial => "DENIED",
                ToolCallOutcome::InvalidRequest => "INVALID_REQUEST",
                ToolCallOutcome::ExecutionFailure => "FAILED",
                ToolCallOutcome::Timeout => "TIMED_OUT",
                ToolCallOutcome::Cancelled => "CANCELLED",
                ToolCallOutcome::Unsupported => "UNSUPPORTED",
            };
            entry.error_code = error_code;
            entry.mutation_applied = match (outcome, entry.mutating) {
                (ToolCallOutcome::Success, Some(false)) => Some(false),
                (ToolCallOutcome::Success, Some(true)) => None,
                (ToolCallOutcome::ExpectedDenial | ToolCallOutcome::InvalidRequest, Some(_)) => {
                    Some(false)
                }
                (_, Some(false)) => Some(false),
                (_, Some(true) | None) => None,
            };
        }
        if active.is_some_and(|_| outcome == ToolCallOutcome::ExpectedDenial) {
            self.denied_count = self.denied_count.saturating_add(1);
        }
        if active.is_some() {
            self.active_call = None;
        }
    }

    fn settle_counters(
        &self,
        sequence: u64,
        successes: &mut u64,
        failures: &mut u64,
        succeeded: bool,
    ) {
        let Some(active) = self
            .active_call
            .as_ref()
            .filter(|active| active.sequence == sequence)
        else {
            return;
        };
        *successes = active.successes_before;
        *failures = active.failures_before;
        if succeeded {
            *successes = successes.saturating_add(1);
        } else {
            *failures = failures.saturating_add(1);
        }
    }

    fn finish_interrupted_call(
        &mut self,
        successes: &mut u64,
        failures: &mut u64,
        failure: Option<&anyhow::Error>,
    ) {
        let Some(active) = self.active_call else {
            return;
        };

        *successes = active.successes_before;
        *failures = active.failures_before.saturating_add(1);
        let (outcome, code) = match failure.map(anyhow::Error::to_string).as_deref() {
            Some("ROLE_EXECUTION_CANCELLED") => {
                (ToolCallOutcome::Cancelled, "ROLE_EXECUTION_CANCELLED")
            }
            Some("ROLE_SUPERVISOR_TIMEOUT") => {
                (ToolCallOutcome::Timeout, "ROLE_SUPERVISOR_TIMEOUT")
            }
            _ => (ToolCallOutcome::ExecutionFailure, "TOOL_EXECUTION_FAILED"),
        };
        self.finish_call(active.sequence, outcome, Some(code));
    }

    fn set_turn_completion(&mut self, completed: bool, callback_count: u64) {
        for entry in &mut self.entries {
            if entry.outcome.is_none() {
                entry.outcome = Some(ToolCallOutcome::ExecutionFailure);
                entry.terminal_state = "PROCESS_EXIT_UNRESOLVED";
                entry.error_code = Some("TOOL_EXECUTION_FAILED");
            }
            entry.turn_completed = Some(completed);
        }

        let unmatched_invocations = self
            .provider_tool_invocations
            .values()
            .filter(|invocation| invocation.invalidated || invocation.callback_count == 0)
            .count() as u64;
        let unmatched_notification_count = unmatched_invocations
            .saturating_add(self.unmatched_provider_updates.len() as u64)
            .saturating_add(self.provider_tool_names_omitted);
        self.unmatched_provider_call_count = unmatched_notification_count;
        self.correlation_partial |= self.unmatched_provider_call_count > 0;
        let capacity = TOOL_CALL_AUDIT_LIMIT.saturating_sub(self.entries.len());
        let mut next_sequence = callback_count.saturating_add(1);
        let unmatched_updates = self
            .provider_tool_invocations
            .values()
            .filter(|invocation| invocation.invalidated || invocation.callback_count == 0)
            .map(|invocation| invocation.observation.clone())
            .chain(self.unmatched_provider_updates.iter().cloned())
            .collect::<Vec<_>>();
        for _ in &unmatched_updates {
            self.record_mutating(None);
        }
        self.mutating_unknown_count = self
            .mutating_unknown_count
            .saturating_add(self.provider_tool_names_omitted);
        for observation in unmatched_updates.iter().take(capacity) {
            self.push_unmatched_provider_call(next_sequence, Some(observation), completed);
            next_sequence = next_sequence.saturating_add(1);
        }
        let recorded_updates = unmatched_updates.len().min(capacity);
        let recorded_overflow = self
            .provider_tool_names_omitted
            .min(capacity.saturating_sub(recorded_updates) as u64);
        for _ in 0..recorded_overflow {
            self.push_unmatched_provider_call(next_sequence, None, completed);
            next_sequence = next_sequence.saturating_add(1);
        }
        self.omitted_count = self.omitted_count.saturating_add(
            unmatched_notification_count
                .saturating_sub(recorded_updates as u64 + recorded_overflow),
        );
    }

    fn push_unmatched_provider_call(
        &mut self,
        sequence: u64,
        observation: Option<&ProviderToolUpdateObservation>,
        completed: bool,
    ) {
        self.entries.push(ToolCallAuditEntry {
            sequence,
            tool_invocation_id: observation.and_then(|update| update.tool_invocation_id.clone()),
            provider_tool_call_id: observation
                .and_then(|update| update.provider_tool_call_id.clone()),
            callback_request_id: None,
            callback_request_id_shape: "not_observed",
            provider_tool_name: "unknown",
            provider_name_mapping: "UNMATCHED",
            provider_update_correlation: "UNMATCHED",
            provider_update_title_class: Some(
                observation.map_or("details_omitted_by_limit", |update| update.title_class),
            ),
            provider_update_tool_kind: Some(
                observation.map_or("details_omitted_by_limit", |update| update.tool_kind),
            ),
            provider_update_status: Some(
                observation.map_or("details_omitted_by_limit", |update| update.status),
            ),
            provider_tool_call_id_shape: Some(
                observation.map_or("details_omitted_by_limit", |update| {
                    update.tool_call_id_shape
                }),
            ),
            canonical_tool_name: "unknown",
            advertised_to_provider: None,
            role_allowed: None,
            outcome: Some(ToolCallOutcome::ExecutionFailure),
            terminal_state: "UNRESOLVED",
            error_code: Some("PROVIDER_CALLBACK_UNRESOLVED"),
            mutating: None,
            mutation_applied: None,
            later_callback_observed: false,
            turn_completed: Some(completed),
            path_arguments: Vec::new(),
        });
    }

    fn metadata(
        &self,
        call_count: u64,
        success_count: u64,
        failure_count: u64,
    ) -> serde_json::Value {
        let provider_notification_count = self.provider_notification_count();
        let correlation_capability = if call_count == 0 && provider_notification_count == 0 {
            "NOT_EXERCISED"
        } else {
            self.correlation_capability()
        };
        serde_json::json!({
            "schema_version": 2,
            "summary": {
                "total": call_count + self.unmatched_notification_count(),
                "callback_count": call_count,
                "provider_notification_count": provider_notification_count,
                "successful": success_count,
                "unsuccessful": failure_count + self.unmatched_notification_count(),
                "mutating": self.mutating_count,
                "mutating_unknown": self.mutating_unknown_count,
                "denied": self.denied_count,
                "unmatched_provider_calls": self.unmatched_provider_call_count,
                "unmatched_callbacks": self.unmatched_callback_count,
            },
            "correlation_capability": correlation_capability,
            "provider_updates": self.provider_update_evidence(),
            "entries": self.entries,
            "omitted_count": self.omitted_count,
            "provider_tool_names_omitted": self.provider_tool_names_omitted,
        })
    }

    fn unmatched_notification_count(&self) -> u64 {
        self.provider_tool_invocations
            .values()
            .filter(|invocation| invocation.invalidated || invocation.callback_count == 0)
            .count() as u64
            + self.unmatched_provider_updates.len() as u64
            + self.provider_tool_names_omitted
    }

    fn provider_notification_count(&self) -> u64 {
        (self.provider_tool_invocations.len() as u64)
            .saturating_add(self.unmatched_provider_updates.len() as u64)
            .saturating_add(self.provider_tool_names_omitted)
    }

    fn correlation_capability(&self) -> &'static str {
        if self.correlation_supported && !self.correlation_partial {
            "SUPPORTED"
        } else if self.correlation_supported || self.correlation_partial {
            "PARTIAL"
        } else {
            "UNSUPPORTED"
        }
    }

    fn provider_update_evidence(&self) -> Vec<ProviderToolUpdateObservation> {
        self.provider_tool_invocations
            .values()
            .map(|invocation| invocation.observation.clone())
            .chain(self.unmatched_provider_updates.iter().cloned())
            .take(PROVIDER_TOOL_NAME_QUEUE_LIMIT)
            .collect()
    }

    fn call_is_correlated(&self, sequence: u64) -> bool {
        self.entries.iter().any(|entry| {
            entry.sequence == sequence && entry.provider_update_correlation == "CORRELATED"
        })
    }
}

fn safe_correlator_id(value: &str, limit: usize) -> bool {
    !value.is_empty()
        && value.len() <= limit
        && value.is_ascii()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

fn safe_json_rpc_request_id(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(value) if safe_correlator_id(value, 128) => {
            Some(format!("s:{value}"))
        }
        serde_json::Value::Number(value)
            if (value.as_i64().is_some() || value.as_u64().is_some())
                && value.to_string().len() <= 128 =>
        {
            Some(format!("n:{value}"))
        }
        _ => None,
    }
}

async fn persist_tool_call_audit(state: &AcpTurnState<'_>) -> Result<()> {
    match (
        state.pool,
        state.agent_exec_id.as_deref(),
        state.role_exec_id.as_deref(),
    ) {
        (Some(pool), Some(agent_exec_id), Some(role_execution_id)) => {
            let audit = state.tool_call_audit.metadata(
                state.tool_calls,
                state.tool_successes,
                state.tool_failures,
            );
            WorkflowStore::new(pool.clone())
                .update_running_agent_tool_audit(agent_exec_id, role_execution_id, &audit)
                .await
        }
        (None, None, None) => Ok(()),
        _ => bail!("AGENT_EXECUTION_AUDIT_OWNER_MISSING"),
    }
}

fn mutation_correlation_permits_dispatch(
    audit: &ToolCallAudit,
    sequence: u64,
    requires_mutation_lock: bool,
) -> bool {
    !requires_mutation_lock || audit.call_is_correlated(sequence)
}

fn classify_provider_update_value(
    value: Option<&serde_json::Value>,
    allowed: &[&'static str],
) -> &'static str {
    match value {
        Some(serde_json::Value::String(value)) => allowed
            .iter()
            .copied()
            .find(|candidate| *candidate == value.as_str())
            .unwrap_or("unrecognized"),
        Some(_) => "non_string",
        None => "missing",
    }
}

fn provider_tool_method_observation(method: &str) -> Option<ProviderToolMethodObservation> {
    use crate::tool_surface::CanonicalToolName as Tool;

    let tool = Tool::from_wire(method)?;
    let orbit_provider_name = match tool {
        Tool::FsReadTextFile => Some("orbit_read_file"),
        Tool::FsWriteTextFile => Some("orbit_write_file"),
        Tool::FsEditFile => Some("orbit_edit_file"),
        Tool::FsListDirectory => Some("orbit_list_directory"),
        Tool::FsFindPath => Some("orbit_find_path"),
        Tool::FsCreateDirectory => Some("orbit_create_directory"),
        Tool::FsMove => Some("orbit_move"),
        Tool::FsCopy => Some("orbit_copy"),
        Tool::FsDeleteFile => Some("orbit_delete_file"),
        Tool::FsDeleteDirectory => Some("orbit_delete_directory"),
        Tool::SearchGrep => Some("orbit_grep"),
        Tool::TerminalCreate => Some("orbit_shell"),
        Tool::TerminalOutput
        | Tool::TerminalWaitForExit
        | Tool::TerminalKill
        | Tool::TerminalRelease => None,
        Tool::GitStatus => Some("orbit_git_status"),
        Tool::GitDiff => Some("orbit_git_diff"),
        Tool::GitShow => Some("orbit_git_show"),
    };
    let slash_name = tool.as_str().replace('.', "/");
    let provider_tool_name = if orbit_provider_name == Some(method) {
        orbit_provider_name.expect("matched above")
    } else if method == tool.as_str() {
        tool.as_str()
    } else if method == tool.legacy_name() {
        tool.legacy_name()
    } else if method == slash_name {
        match tool {
            Tool::FsReadTextFile => "fs/read_text_file",
            Tool::FsWriteTextFile => "fs/write_text_file",
            Tool::FsEditFile => "fs/edit_file",
            Tool::FsListDirectory => "fs/list_directory",
            Tool::FsFindPath => "fs/find_path",
            Tool::FsCreateDirectory => "fs/create_directory",
            Tool::FsMove => "fs/move",
            Tool::FsCopy => "fs/copy",
            Tool::FsDeleteFile => "fs/delete_file",
            Tool::FsDeleteDirectory => "fs/delete_directory",
            Tool::SearchGrep => "search/grep",
            Tool::TerminalCreate => "terminal/create",
            Tool::TerminalOutput => "terminal/output",
            Tool::TerminalWaitForExit => "terminal/wait_for_exit",
            Tool::TerminalKill => "terminal/kill",
            Tool::TerminalRelease => "terminal/release",
            Tool::GitStatus => "git/status",
            Tool::GitDiff => "git/diff",
            Tool::GitShow => "git/show",
        }
    } else {
        return None;
    };
    Some(ProviderToolMethodObservation {
        provider_tool_name,
        request_tool_name: tool.legacy_name(),
        canonical_tool_name: tool,
    })
}

fn known_tool_error_code(message: &str) -> Option<&'static str> {
    [
        crate::tool_surface::ERR_PATH_NOT_FOUND,
        crate::tool_surface::ERR_PATH_OUTSIDE_WORKSPACE,
        crate::tool_surface::ERR_DESTINATION_EXISTS,
        crate::tool_surface::ERR_READ_ONLY_ROLE,
        crate::tool_surface::ERR_MUTATION_LOCK_REQUIRED,
        crate::tool_surface::ERR_OUTPUT_TRUNCATED,
        crate::tool_surface::ERR_COMMAND_TIMEOUT,
        crate::tool_surface::ERR_PROCESS_NOT_FOUND,
        crate::tool_surface::ERR_UNSUPPORTED_TOOL,
        crate::tool_surface::ERR_ROLE_NOT_ALLOWED,
        crate::tool_surface::ERR_WORKSPACE_IDENTITY_REQUIRED,
        crate::tool_surface::ERR_TOOL_CALL_LIMIT,
        crate::tool_surface::ERR_OUTPUT_LIMIT,
        crate::tool_surface::ERR_NO_MATCH,
        crate::tool_surface::ERR_MULTIPLE_MATCHES,
        "CLI_WORKFLOW_TERMINAL_DISABLED",
        "INVALID_REQUEST",
        "TOOL_EXECUTION_FAILED",
        "TOOL_RESPONSE_FAILED",
        "ROLE_EXECUTION_CANCELLED",
        "ROLE_SUPERVISOR_TIMEOUT",
        "PROVIDER_CALLBACK_UNRESOLVED",
        "TOOL_AUTHORIZATION_DENIED",
    ]
    .into_iter()
    .find(|code| message.contains(code))
}

fn normalized_tool_error_code(error: &anyhow::Error) -> &'static str {
    for cause in error.chain() {
        let message = cause.to_string();
        if let Some(code) = known_tool_error_code(&message) {
            return code;
        }
        if cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            || message.contains("No such file or directory")
            || message.contains("os error 2")
        {
            return crate::tool_surface::ERR_PATH_NOT_FOUND;
        }
        if message.contains("outside workspace")
            || message.contains("path traversal rejected")
            || message.contains("absolute path rejected")
        {
            return crate::tool_surface::ERR_PATH_OUTSIDE_WORKSPACE;
        }
    }
    "TOOL_EXECUTION_FAILED"
}

fn tool_request_has_valid_required_args(
    tool: crate::tool_surface::CanonicalToolName,
    params: &serde_json::Value,
) -> bool {
    use crate::tool_surface::CanonicalToolName as Tool;
    let required: &[(&str, bool)] = match tool {
        Tool::FsReadTextFile
        | Tool::FsCreateDirectory
        | Tool::FsDeleteFile
        | Tool::FsDeleteDirectory => &[("path", true)],
        Tool::FsWriteTextFile => &[("path", true), ("content", false)],
        Tool::FsEditFile => &[("path", true), ("old_text", true), ("new_text", false)],
        Tool::FsMove | Tool::FsCopy => &[("source", true), ("destination", true)],
        Tool::SearchGrep => &[("query", true)],
        Tool::FsFindPath => &[("pattern", false)],
        _ => return true,
    };
    required.iter().all(|(field, nonempty)| {
        params
            .get(*field)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !*nonempty || !value.is_empty())
    })
}

fn path_arguments_for_tool(
    tool: crate::tool_surface::CanonicalToolName,
    params: &serde_json::Value,
    repo_path: &Path,
) -> Vec<PathArgumentAudit> {
    use crate::tool_surface::CanonicalToolName as Tool;

    let fields: &[(&str, bool)] = match tool {
        Tool::FsReadTextFile
        | Tool::FsWriteTextFile
        | Tool::FsEditFile
        | Tool::FsCreateDirectory
        | Tool::FsDeleteFile
        | Tool::FsDeleteDirectory => &[("path", true)],
        Tool::FsListDirectory
        | Tool::FsFindPath
        | Tool::SearchGrep
        | Tool::GitStatus
        | Tool::GitDiff
        | Tool::GitShow => &[("path", false)],
        Tool::FsMove | Tool::FsCopy => &[("source", true), ("destination", true)],
        _ => &[],
    };

    fields
        .iter()
        .filter_map(|(argument, required)| {
            let value = params.get(*argument);
            if !required && value.is_none_or(serde_json::Value::is_null) {
                return None;
            }
            Some(match value.and_then(serde_json::Value::as_str) {
                Some(value) => classify_path_argument(argument, value, repo_path),
                None => invalid_path_argument(argument),
            })
        })
        .collect()
}

fn invalid_path_argument(argument: &'static str) -> PathArgumentAudit {
    PathArgumentAudit {
        argument,
        state: PathAuditState::InvalidPath,
        workspace_relative_path: None,
        exists: None,
        display_truncated: false,
    }
}

fn outside_workspace_path_argument(argument: &'static str) -> PathArgumentAudit {
    PathArgumentAudit {
        argument,
        state: PathAuditState::OutsideWorkspaceRedacted,
        workspace_relative_path: None,
        exists: None,
        display_truncated: false,
    }
}

fn classify_path_argument(
    argument: &'static str,
    requested: &str,
    repo_path: &Path,
) -> PathArgumentAudit {
    if requested.is_empty() || requested.len() > TOOL_PATH_INPUT_LIMIT || requested.contains('\0') {
        return invalid_path_argument(argument);
    }

    let Ok(canonical_root) = repo_path.canonicalize() else {
        return invalid_path_argument(argument);
    };
    let requested_path = Path::new(requested);
    let windows_absolute = requested.as_bytes().get(1) == Some(&b':')
        && requested
            .as_bytes()
            .get(2)
            .is_some_and(|separator| matches!(separator, b'/' | b'\\'));
    if (requested_path.is_absolute() || windows_absolute)
        && !requested.starts_with("/orbit/home/")
        && requested != "/orbit/home"
        && requested_path.strip_prefix(&canonical_root).is_err()
        && requested_path.strip_prefix(repo_path).is_err()
    {
        return outside_workspace_path_argument(argument);
    }

    let confined_path = match crate::fs_tools::confine_path(repo_path, requested, false, true) {
        Ok(path) => path,
        Err(error) if path_error_indicates_workspace_escape(&error) => {
            return outside_workspace_path_argument(argument);
        }
        Err(_) => return invalid_path_argument(argument),
    };
    let Ok(relative_path) = confined_path.strip_prefix(&canonical_root) else {
        return invalid_path_argument(argument);
    };
    let Some(relative_text) = safe_relative_path_display(relative_path) else {
        return invalid_path_argument(argument);
    };
    let exists = match confined_path.symlink_metadata() {
        Ok(_) => Some(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    };
    let (relative_text, display_truncated) = bounded_path_display(relative_text);

    PathArgumentAudit {
        argument,
        state: PathAuditState::WorkspaceRelative,
        workspace_relative_path: Some(relative_text),
        exists,
        display_truncated,
    }
}

fn path_error_indicates_workspace_escape(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        let message = cause.to_string();
        message.contains("escapes workspace") || message.contains("outside workspace")
    })
}

fn safe_relative_path_display(path: &Path) -> Option<String> {
    let mut encoded = String::new();
    for component in path.components() {
        let std::path::Component::Normal(component) = component else {
            if matches!(component, std::path::Component::CurDir) {
                continue;
            }
            return None;
        };
        if !encoded.is_empty() {
            encoded.push('/');
        }
        for byte in component.to_string_lossy().as_bytes() {
            if byte.is_ascii_alphanumeric() || matches!(*byte, b'.' | b'_' | b'-') {
                encoded.push(*byte as char);
            } else {
                use std::fmt::Write as _;
                let _ = write!(encoded, "%{byte:02X}");
            }
        }
    }
    if encoded.is_empty() {
        Some(".".into())
    } else {
        Some(encoded)
    }
}

fn bounded_path_display(mut path: String) -> (String, bool) {
    if path.len() <= TOOL_PATH_DISPLAY_LIMIT {
        return (path, false);
    }
    path.truncate(TOOL_PATH_DISPLAY_LIMIT - 3);
    path.push_str("...");
    (path, true)
}

fn audit_bool(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "true",
        Some(false) => "false",
        None => "unknown",
    }
}

fn audit_outcome(value: &serde_json::Value) -> (&'static str, &'static str) {
    match value.as_str() {
        Some("SUCCESS") => ("SUCCESS", "Tool call completed."),
        Some("EXPECTED_DENIAL") => ("EXPECTED_DENIAL", "Request denied by policy."),
        Some("INVALID_REQUEST") => ("INVALID_REQUEST", "Tool request was invalid."),
        Some("TIMEOUT") => ("TIMEOUT", "Tool execution timed out."),
        Some("CANCELLED") => ("CANCELLED", "Tool call was cancelled."),
        Some("UNSUPPORTED") => ("UNSUPPORTED", "Provider tool request is unsupported."),
        Some("EXECUTION_FAILURE") => ("EXECUTION_FAILURE", "Tool execution failed."),
        _ => ("PENDING", "Diagnostic unavailable."),
    }
}

fn render_path_arguments(value: &serde_json::Value) -> String {
    let Some(arguments) = value.as_array() else {
        return "-".into();
    };
    arguments
        .iter()
        .take(2)
        .filter_map(|argument| {
            let name = match argument.get("argument").and_then(serde_json::Value::as_str) {
                Some("path") => "path",
                Some("source") => "source",
                Some("destination") => "destination",
                _ => return None,
            };
            let rendered = match argument.get("state").and_then(serde_json::Value::as_str) {
                Some("WORKSPACE_RELATIVE") => {
                    let Some(path) = argument
                        .get("workspace_relative_path")
                        .and_then(serde_json::Value::as_str)
                        .filter(|path| safe_audit_relative_path(path))
                    else {
                        return Some(format!("{name}=INVALID_PATH"));
                    };
                    let exists =
                        audit_bool(argument.get("exists").and_then(serde_json::Value::as_bool));
                    format!("{name}=WORKSPACE_RELATIVE({path}; exists={exists})")
                }
                Some("OUTSIDE_WORKSPACE_REDACTED") => {
                    format!("{name}=OUTSIDE_WORKSPACE_REDACTED")
                }
                // Preserve a safe label for audit rows written by older candidates.
                Some("UNNORMALIZED_GIT_PATH_FILTER") => format!("{name}=INVALID_PATH"),
                _ => format!("{name}=INVALID_PATH"),
            };
            Some(rendered)
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn safe_audit_relative_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= TOOL_PATH_DISPLAY_LIMIT
        && !path.starts_with('/')
        && path.is_ascii()
        && path.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/' | b'%')
        })
        && path.split('/').all(|component| component != "..")
}

/// Renders only bounded, allowlisted audit fields; provider payloads are never shown.
pub fn render_tool_call_audit(metadata: &serde_json::Value) -> String {
    let Some(audit) = metadata.get("tool_call_audit") else {
        return "Tool-call audit unavailable.".into();
    };
    let summary = audit.get("summary");
    let count = |name| {
        summary
            .and_then(|summary| summary.get(name))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    };
    let entries = audit
        .get("entries")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let provider_updates = audit
        .get("provider_updates")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let omitted = audit
        .get("omitted_count")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let provider_titles_omitted = audit
        .get("provider_tool_names_omitted")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let capability = match audit
        .get("correlation_capability")
        .and_then(serde_json::Value::as_str)
    {
        Some("SUPPORTED") => "SUPPORTED",
        Some("PARTIAL") => "PARTIAL",
        Some("NOT_EXERCISED") => "NOT_EXERCISED",
        _ => "UNSUPPORTED",
    };
    let correlated_terminal_rows = entries
        .iter()
        .filter(|entry| {
            entry
                .get("provider_update_correlation")
                .and_then(serde_json::Value::as_str)
                == Some("CORRELATED")
                && entry
                    .get("terminal_state")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|state| state != "DISPATCHED" && state != "UNRESOLVED")
        })
        .count() as u64;
    let unmatched_provider_calls = count("unmatched_provider_calls");
    let unmatched_callbacks = count("unmatched_callbacks");
    let unresolved = entries
        .iter()
        .filter(|entry| {
            entry
                .get("provider_update_correlation")
                .and_then(serde_json::Value::as_str)
                != Some("CORRELATED")
                || entry
                    .get("terminal_state")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|state| {
                        matches!(
                            state,
                            "DISPATCHED" | "UNRESOLVED" | "PROCESS_EXIT_UNRESOLVED"
                        )
                    })
        })
        .count() as u64;
    let recorded_provider_notifications = provider_updates.len() as u64;
    let provider_notifications = summary
        .and_then(|summary| summary.get("provider_notification_count"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_else(|| recorded_provider_notifications.saturating_add(provider_titles_omitted));
    let callbacks = summary
        .and_then(|summary| summary.get("callback_count"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_else(|| count("total").saturating_sub(unmatched_provider_calls));
    let mut report = format!(
        "Tool-call audit: correlation={capability}, provider notifications={provider_notifications} (recorded={recorded_provider_notifications}, omitted={provider_titles_omitted}), callbacks={callbacks}, correlated terminal rows={correlated_terminal_rows}, unresolved={unresolved} (provider={}, callbacks={}), total={}, successful={}, unsuccessful={}, mutating={}, denied={}, omitted={omitted}, provider titles omitted={provider_titles_omitted}\n",
        unmatched_provider_calls,
        unmatched_callbacks,
        count("total"),
        count("successful"),
        count("unsuccessful"),
        count("mutating"),
        count("denied")
    );
    report.push_str("seq | ToolInvocationId | provider ToolCall ID | callback JSON-RPC ID | provider callback method | update correlation | title class | provider kind | provider status | toolCallId shape | mapping | canonical | advertised | role allowed | outcome | terminal state | error code | mutation applied | later callback observed | turn complete | paths | detail\n");

    for entry in entries.iter().take(TOOL_CALL_AUDIT_LIMIT) {
        let string = |name| {
            entry
                .get(name)
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
        };
        let report_id =
            |name: &str, limit: usize| match entry.get(name).and_then(serde_json::Value::as_str) {
                Some(value) if safe_correlator_id(value, limit) => value,
                Some(_) => "redacted",
                None => "-",
            };
        let provider = provider_tool_method_observation(string("provider_tool_name"))
            .map_or("unknown", |name| name.provider_tool_name);
        let canonical =
            crate::tool_surface::CanonicalToolName::from_canonical(string("canonical_tool_name"))
                .map_or("unknown", |tool| tool.as_str());
        let mapping = match string("provider_name_mapping") {
            "MATCH" => "MATCH",
            "MISMATCH" => "MISMATCH",
            "UNMATCHED" => "UNMATCHED",
            _ => "UNKNOWN",
        };
        let update_correlation = match string("provider_update_correlation") {
            "CORRELATED" => "CORRELATED",
            "UNMATCHED" => "UNMATCHED",
            "CALLBACK_WITHOUT_INVOCATION_ID"
            | "CALLBACK_WITHOUT_UPDATE"
            | "PROVIDER_ID_MISMATCH"
            | "INVALIDATED_INVOCATION"
            | "DUPLICATE_CALLBACK_ID"
            | "DUPLICATE_INVOCATION_CALLBACK"
            | "CALLBACK_TRACKING_LIMIT_EXCEEDED"
            | "INVALID_CALLBACK_ID"
            | "MALFORMED" => "UNRESOLVED",
            _ => "UNKNOWN",
        };
        let title_class = match string("provider_update_title_class") {
            "non_empty_string" => "non_empty_string",
            "empty_string" => "empty_string",
            "non_string" => "non_string",
            "missing" => "missing",
            "update_not_observed" => "update_not_observed",
            "details_omitted_by_limit" => "details_omitted_by_limit",
            _ => "unknown",
        };
        let provider_kind = match string("provider_update_tool_kind") {
            "read"
            | "edit"
            | "delete"
            | "move"
            | "search"
            | "execute"
            | "think"
            | "fetch"
            | "switch_mode"
            | "other"
            | "unrecognized"
            | "non_string"
            | "missing"
            | "update_not_observed"
            | "details_omitted_by_limit" => string("provider_update_tool_kind"),
            _ => "unknown",
        };
        let provider_status = match string("provider_update_status") {
            "pending"
            | "in_progress"
            | "completed"
            | "failed"
            | "cancelled"
            | "canceled"
            | "unrecognized"
            | "non_string"
            | "missing"
            | "update_not_observed"
            | "details_omitted_by_limit" => string("provider_update_status"),
            _ => "unknown",
        };
        let tool_call_id_shape = match string("provider_tool_call_id_shape") {
            "string"
            | "empty_string"
            | "oversized_string"
            | "duplicate_string"
            | "deduplication_limit"
            | "non_string"
            | "missing"
            | "update_not_observed"
            | "details_omitted_by_limit" => string("provider_tool_call_id_shape"),
            _ => "unknown",
        };
        let (outcome, detail) =
            audit_outcome(entry.get("outcome").unwrap_or(&serde_json::Value::Null));
        let error_code = known_tool_error_code(string("error_code")).unwrap_or("-");
        let mutating = audit_bool(entry.get("mutating").and_then(serde_json::Value::as_bool));
        let applied = audit_bool(
            entry
                .get("mutation_applied")
                .and_then(serde_json::Value::as_bool),
        );
        let later = audit_bool(
            entry
                .get("later_callback_observed")
                .and_then(serde_json::Value::as_bool),
        );
        let complete = audit_bool(
            entry
                .get("turn_completed")
                .and_then(serde_json::Value::as_bool),
        );
        let paths = render_path_arguments(
            entry
                .get("path_arguments")
                .unwrap_or(&serde_json::Value::Null),
        );
        report.push_str(&format!(
            "{} | {} | {} | {} | {provider} | {update_correlation} | {title_class} | {provider_kind} | {provider_status} | {tool_call_id_shape} | {mapping} | {canonical} | {} | {} | {outcome} | {} | {error_code} | {mutating}/{applied} | {later} | {complete} | {paths} | {}\n",
            entry.get("sequence").and_then(serde_json::Value::as_u64).unwrap_or(0),
            report_id("tool_invocation_id", 128),
            report_id("provider_tool_call_id", 256),
            report_id("callback_request_id", 130),
            audit_bool(entry.get("advertised_to_provider").and_then(serde_json::Value::as_bool)),
            audit_bool(entry.get("role_allowed").and_then(serde_json::Value::as_bool)),
            entry.get("terminal_state").and_then(serde_json::Value::as_str).filter(|value| matches!(*value, "DISPATCHED" | "SUCCESS" | "DENIED" | "INVALID_REQUEST" | "FAILED" | "TIMED_OUT" | "CANCELLED" | "UNSUPPORTED" | "PROCESS_EXIT_UNRESOLVED" | "UNRESOLVED")).unwrap_or("unknown"),
            detail,
        ));
    }
    report
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
    let is_session_update =
        message.get("method").and_then(serde_json::Value::as_str) == Some("session/update");
    let persist_audit = if is_session_update {
        is_provider_tool_call_update(&message)
    } else {
        message.get("id").is_some()
    };
    let result = handle_acp_message_inner(wire, state, message).await;
    if persist_audit {
        let persist_result = persist_tool_call_audit(state).await;
        if result.is_ok() {
            persist_result?;
        } else {
            // Keep the primary protocol/operation failure while still attempting
            // to record its terminal or unresolved audit state.
            persist_result?;
        }
    }
    result
}

fn is_provider_tool_call_update(message: &serde_json::Value) -> bool {
    if message.get("method").and_then(serde_json::Value::as_str) != Some("session/update") {
        return false;
    }
    let Some(params) = message.get("params") else {
        return false;
    };
    params
        .get("update")
        .unwrap_or(params)
        .get("sessionUpdate")
        .and_then(serde_json::Value::as_str)
        == Some("tool_call")
}

async fn handle_acp_message_inner(
    wire: &mut Wire,
    state: &mut AcpTurnState<'_>,
    message: serde_json::Value,
) -> Result<()> {
    if let Some(method) = message.get("method").and_then(|m| m.as_str()) {
        if method == "session/update" {
            if let Some(params) = message.get("params") {
                if let Some(update) = params.get("update") {
                    state
                        .tool_call_audit
                        .observe_provider_tool_name_with_metadata(
                            update,
                            crate::acp_wire::orbit_tool_invocation_meta(&message),
                        );
                    extract_text_from_json(update, &mut state.agent_output);
                } else {
                    state
                        .tool_call_audit
                        .observe_provider_tool_name_with_metadata(
                            params,
                            crate::acp_wire::orbit_tool_invocation_meta(&message),
                        );
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
        state.tool_calls = state.tool_calls.saturating_add(1);
        state.tool_call_audit.set_role_id(state.role_id.as_deref());
        let invocation_metadata = crate::acp_wire::orbit_tool_invocation_meta(&message);
        let audit_sequence = state.tool_call_audit.begin_call_with_context(
            state.tool_calls,
            method,
            message.get("id"),
            invocation_metadata,
            canonical,
            state.tool_successes,
            state.tool_failures,
        );
        // Persist the DISPATCHED row before any callback authorization or
        // mutation branch. A crash or timeout from here remains unresolved.
        persist_tool_call_audit(state).await?;
        let Some(tool) = canonical else {
            state.tool_failures = state.tool_failures.saturating_add(1);
            *state.tool_counts.entry("unsupported".into()).or_insert(0) += 1;
            state.tool_call_audit.finish_call(
                audit_sequence,
                ToolCallOutcome::Unsupported,
                Some(crate::tool_surface::ERR_UNSUPPORTED_TOOL),
            );
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

        *state
            .tool_counts
            .entry(tool.legacy_name().into())
            .or_insert(0) += 1;
        *state.tool_counts.entry(tool.as_str().into()).or_insert(0) += 1;

        let params = message
            .get("params")
            .cloned()
            .unwrap_or(serde_json::json!({}));
        state
            .tool_call_audit
            .record_path_arguments(audit_sequence, tool, &params, state.repo_path);
        persist_tool_call_audit(state).await?;

        if matches!(
            tool,
            crate::tool_surface::CanonicalToolName::TerminalCreate
                | crate::tool_surface::CanonicalToolName::TerminalOutput
                | crate::tool_surface::CanonicalToolName::TerminalWaitForExit
                | crate::tool_surface::CanonicalToolName::TerminalKill
                | crate::tool_surface::CanonicalToolName::TerminalRelease
        ) {
            state.tool_failures = state.tool_failures.saturating_add(1);
            state.tool_call_audit.finish_call(
                audit_sequence,
                ToolCallOutcome::ExpectedDenial,
                Some("CLI_WORKFLOW_TERMINAL_DISABLED"),
            );
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
                state.tool_failures = state.tool_failures.saturating_add(1);
                let error_text = error.to_string();
                state.tool_call_audit.finish_call(
                    audit_sequence,
                    ToolCallOutcome::ExpectedDenial,
                    Some(known_tool_error_code(&error_text).unwrap_or("TOOL_AUTHORIZATION_DENIED")),
                );
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
                state.tool_failures = state.tool_failures.saturating_add(1);
                state.tool_call_audit.finish_call(
                    audit_sequence,
                    ToolCallOutcome::ExpectedDenial,
                    Some(crate::tool_surface::ERR_MUTATION_LOCK_REQUIRED),
                );
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
                state.tool_failures = state.tool_failures.saturating_add(1);
                state.tool_call_audit.finish_call(
                    audit_sequence,
                    ToolCallOutcome::ExpectedDenial,
                    Some(crate::tool_surface::ERR_MUTATION_LOCK_REQUIRED),
                );
                wire.response_error(
                    req_id,
                    -32603,
                    crate::tool_surface::ERR_MUTATION_LOCK_REQUIRED,
                )
                .await?;
                return Ok(());
            }
        }

        if !mutation_correlation_permits_dispatch(
            &state.tool_call_audit,
            audit_sequence,
            meta.requires_mutation_lock,
        ) {
            state.tool_failures = state.tool_failures.saturating_add(1);
            state.tool_call_audit.finish_call(
                audit_sequence,
                ToolCallOutcome::ExecutionFailure,
                Some("PROVIDER_CALLBACK_UNRESOLVED"),
            );
            wire.response_error(req_id, -32603, "PROVIDER_CALLBACK_UNRESOLVED")
                .await?;
            return Ok(());
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
                            state
                                .tool_call_audit
                                .record_operation_error(audit_sequence, &e);
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
                            state
                                .tool_call_audit
                                .record_operation_error(audit_sequence, &e);
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
                            state
                                .tool_call_audit
                                .record_operation_error(audit_sequence, &e);
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
                            state
                                .tool_call_audit
                                .record_operation_error(audit_sequence, &e);
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
                            state
                                .tool_call_audit
                                .record_operation_error(audit_sequence, &e);
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
                            state
                                .tool_call_audit
                                .record_operation_error(audit_sequence, &e);
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
                            state
                                .tool_call_audit
                                .record_operation_error(audit_sequence, &e);
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
                            state
                                .tool_call_audit
                                .record_operation_error(audit_sequence, &e);
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
                            state
                                .tool_call_audit
                                .record_operation_error(audit_sequence, &e);
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
                            state
                                .tool_call_audit
                                .record_operation_error(audit_sequence, &e);
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
                            state
                                .tool_call_audit
                                .record_operation_error(audit_sequence, &e);
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
                            state
                                .tool_call_audit
                                .record_operation_error(audit_sequence, &e);
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
                            state
                                .tool_call_audit
                                .record_operation_error(audit_sequence, &e);
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
                            state
                                .tool_call_audit
                                .record_operation_error(audit_sequence, &e);
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
                                state
                                    .tool_call_audit
                                    .record_operation_error(audit_sequence, &e);
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
                            state
                                .tool_call_audit
                                .record_operation_error(audit_sequence, &e);
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
                                state
                                    .tool_call_audit
                                    .record_operation_error(audit_sequence, &e);
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
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                state.tool_call_audit.settle_counters(
                    audit_sequence,
                    &mut state.tool_successes,
                    &mut state.tool_failures,
                    false,
                );
                state.tool_call_audit.finish_call(
                    audit_sequence,
                    ToolCallOutcome::ExecutionFailure,
                    Some("TOOL_RESPONSE_FAILED"),
                );
                return Err(error);
            }
            Err(_) => {
                state.tool_failures = state.tool_failures.saturating_add(1);
                state.tool_call_audit.settle_counters(
                    audit_sequence,
                    &mut state.tool_successes,
                    &mut state.tool_failures,
                    false,
                );
                state.tool_call_audit.finish_call(
                    audit_sequence,
                    ToolCallOutcome::Timeout,
                    Some(crate::tool_surface::ERR_COMMAND_TIMEOUT),
                );
                wire.response_error(
                    timeout_request_id,
                    -32603,
                    crate::tool_surface::ERR_COMMAND_TIMEOUT,
                )
                .await?;
                return Ok(());
            }
        }
        let response_limit_hit = wire.take_response_limit_hit();
        if response_limit_hit {
            if state.tool_successes > successes_before {
                state.tool_successes -= 1;
            }
            if state.tool_failures == failures_before {
                state.tool_failures += 1;
            }
        }

        let succeeded = !response_limit_hit
            && state.tool_successes > successes_before
            && state.tool_failures == failures_before;
        let outcome = if succeeded {
            (ToolCallOutcome::Success, None)
        } else if response_limit_hit {
            (
                ToolCallOutcome::ExecutionFailure,
                Some(crate::tool_surface::ERR_OUTPUT_LIMIT),
            )
        } else if !tool_request_has_valid_required_args(tool, &params) {
            (ToolCallOutcome::InvalidRequest, Some("INVALID_REQUEST"))
        } else {
            (ToolCallOutcome::ExecutionFailure, None)
        };
        state.tool_call_audit.settle_counters(
            audit_sequence,
            &mut state.tool_successes,
            &mut state.tool_failures,
            succeeded,
        );
        state
            .tool_call_audit
            .finish_call(audit_sequence, outcome.0, outcome.1);
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

#[derive(Debug, Clone, serde::Serialize)]
struct AcpRoleLifecycle {
    schema_version: u8,
    phase: &'static str,
    attempted_phase: &'static str,
    failed_phase: Option<&'static str>,
    last_confirmed_phase: &'static str,
    milestones: Vec<&'static str>,
    outcome: &'static str,
    normalized_reason: Option<&'static str>,
    prompt_uncertainty: &'static str,
    cleanup_state: &'static str,
    persistence_state: &'static str,
    process_exit_code: Option<i32>,
    process_signal: Option<i32>,
    supervisor_outcome: &'static str,
    supervisor_failure: Option<&'static str>,
    supervisor_receipt: Option<crate::acp_process::CleanupReceiptEvidence>,
    tool_audit_applicability: &'static str,
    #[serde(skip)]
    finalization_attempted: bool,
}

impl AcpRoleLifecycle {
    fn new() -> Self {
        Self {
            schema_version: 1,
            phase: "AGENT_EXECUTION_CREATED",
            attempted_phase: "AGENT_EXECUTION_CREATED",
            failed_phase: None,
            last_confirmed_phase: "AGENT_EXECUTION_CREATED",
            milestones: vec!["TARGET_COMMITTED", "AGENT_EXECUTION_CREATED"],
            outcome: "IN_PROGRESS",
            normalized_reason: None,
            prompt_uncertainty: "NOT_DISPATCHED",
            cleanup_state: "NO_RUNTIME_RESOURCE_CREATED",
            persistence_state: "CONFIRMED",
            process_exit_code: None,
            process_signal: None,
            supervisor_outcome: "NOT_OBSERVED",
            supervisor_failure: None,
            supervisor_receipt: None,
            tool_audit_applicability: "NOT_APPLICABLE_BEFORE_TOOL_PHASE",
            finalization_attempted: false,
        }
    }

    fn enter(&mut self, phase: &'static str) {
        self.phase = phase;
        self.attempted_phase = phase;
    }

    fn confirm(&mut self, phase: &'static str) {
        self.phase = phase;
        self.attempted_phase = phase;
        self.last_confirmed_phase = phase;
        if self.milestones.last().copied() != Some(phase) && self.milestones.len() < 16 {
            self.milestones.push(phase);
        }
    }

    fn note_tool_activity(&mut self) {
        self.tool_audit_applicability = "APPLICABLE";
        const OBSERVED: &str = "TOOL_ACTIVITY_OBSERVED";
        if !self.milestones.contains(&OBSERVED) && self.milestones.len() < 16 {
            self.milestones.push(OBSERVED);
        }
    }

    fn normalized_failure(&self, error: &anyhow::Error) -> &'static str {
        if error.is::<RoleLifecyclePersistenceFailed>() {
            return "AGENT_EXECUTION_LIFECYCLE_PERSISTENCE_FAILED";
        }
        if error.is::<RoleTerminalPersistenceUnconfirmed>() {
            return "AGENT_EXECUTION_TERMINAL_PERSISTENCE_UNCONFIRMED";
        }
        if let Some(error) = error.downcast_ref::<RoleSupervisorOutcomeFailure>() {
            return error.0;
        }
        if error.is::<RoleHandoffResponseMissing>() {
            return "HANDOFF_RESPONSE_MISSING";
        }
        if error.is::<RoleTerminalCleanupFailed>() {
            return "TERMINAL_CLEANUP_FAILED";
        }
        if error.is::<crate::acp_runtime::TurnTimeout>() {
            return "ACP_PROMPT_TIMEOUT";
        }
        if error.is::<RoleExecutionCancelled>() {
            return "ROLE_EXECUTION_CANCELLED";
        }
        if error.is::<RoleSupervisorTimeout>() {
            return "SUPERVISOR_TIMEOUT";
        }
        match self.phase {
            "CREDENTIAL_RESOLUTION" => "CREDENTIAL_RESOLUTION_FAILED",
            "CREDENTIAL_STAGING" => "CREDENTIAL_STAGING_FAILED",
            "RUNTIME_PREPARATION" => "RUNTIME_PREPARATION_FAILED",
            "SUPERVISOR_START" => "SUPERVISOR_START_FAILED",
            "ACP_INITIALIZE" => "ACP_INITIALIZE_FAILED",
            "SESSION_CREATION" => "ACP_SESSION_CREATION_FAILED",
            "PROMPT_DISPATCH" | "PROMPT_IN_FLIGHT" => "ACP_PROMPT_FAILED",
            "CLEANUP" => "ACP_CLEANUP_FAILED",
            "HANDOFF" => "HANDOFF_PARSE_FAILED",
            _ => "ACP_EXECUTION_FAILED",
        }
    }

    fn record_error(&mut self, error: &anyhow::Error) {
        let reason = self.normalized_failure(error);
        self.failed_phase = Some(self.phase);
        if reason == "ACP_PROMPT_TIMEOUT" {
            let pending = error
                .downcast_ref::<crate::acp_runtime::TurnTimeout>()
                .and_then(|timeout| timeout.pending_model_call);
            self.prompt_uncertainty = match pending {
                Some(true) => "UNRESOLVED_PENDING_MODEL_CALL",
                Some(false) => "UNRESOLVED_NO_PENDING_MODEL_CALL",
                None => "UNRESOLVED_UNKNOWN",
            };
        } else if self.prompt_uncertainty == "IN_FLIGHT" {
            self.prompt_uncertainty = "UNRESOLVED_UNKNOWN";
        }
        self.outcome = "FAILED";
        self.normalized_reason = Some(reason);
        self.phase = "TERMINAL";
    }

    fn finish_success(&mut self) {
        if self.prompt_uncertainty == "IN_FLIGHT" {
            self.prompt_uncertainty = "RESOLVED";
        }
        self.outcome = "SUCCEEDED";
        self.normalized_reason = Some("COMPLETED");
        self.phase = "TERMINAL";
        self.attempted_phase = "TERMINAL";
    }

    fn restore_after_persistence_failure(
        &mut self,
        previous: &Self,
        attempted_phase: &'static str,
    ) {
        *self = previous.clone();
        self.persistence_state = "UNCONFIRMED";
        self.enter(attempted_phase);
    }

    fn preserve_cleanup_state_for_failure_finalization(&mut self) {
        if !matches!(
            self.cleanup_state,
            "NO_RUNTIME_RESOURCE_CREATED" | "CONFIRMED"
        ) {
            self.cleanup_state = "UNCONFIRMED";
        }
    }

    fn needs_failure_finalization(&self) -> bool {
        !self.finalization_attempted
    }

    fn note_terminal_persistence_attempt(&mut self) {
        self.finalization_attempted = true;
    }

    fn note_terminal_persistence_failure(&mut self) {
        self.finalization_attempted = true;
        self.persistence_state = "UNCONFIRMED";
    }

    fn value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("bounded ACP lifecycle serializes")
    }
}

#[derive(Debug)]
struct SupervisorEvidence {
    exit_code: Option<i32>,
    signal: Option<i32>,
    cleanup_confirmed: bool,
    failure: Option<&'static str>,
    receipt: Option<crate::acp_process::CleanupReceiptEvidence>,
}

#[derive(Debug)]
struct RoleExecutionCancelled;

impl std::fmt::Display for RoleExecutionCancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("role execution cancelled")
    }
}

impl std::error::Error for RoleExecutionCancelled {}

#[derive(Debug)]
struct RoleSupervisorTimeout;

impl std::fmt::Display for RoleSupervisorTimeout {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("role supervisor deadline elapsed")
    }
}

impl std::error::Error for RoleSupervisorTimeout {}

#[derive(Debug)]
struct RoleLifecyclePersistenceFailed;

impl std::fmt::Display for RoleLifecyclePersistenceFailed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("agent execution lifecycle evidence persistence failed")
    }
}

impl std::error::Error for RoleLifecyclePersistenceFailed {}

#[derive(Debug)]
struct RoleTerminalPersistenceUnconfirmed;

impl std::fmt::Display for RoleTerminalPersistenceUnconfirmed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("terminal AgentExecution persistence is unconfirmed")
    }
}

impl std::error::Error for RoleTerminalPersistenceUnconfirmed {}

#[derive(Debug)]
struct RoleSupervisorOutcomeFailure(&'static str);

impl std::fmt::Display for RoleSupervisorOutcomeFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for RoleSupervisorOutcomeFailure {}

#[derive(Debug)]
struct RoleHandoffResponseMissing;

impl std::fmt::Display for RoleHandoffResponseMissing {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("handoff response envelope missing")
    }
}

impl std::error::Error for RoleHandoffResponseMissing {}

#[derive(Debug)]
struct RoleTerminalCleanupFailed;

impl std::fmt::Display for RoleTerminalCleanupFailed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("terminal cleanup failed")
    }
}

impl std::error::Error for RoleTerminalCleanupFailed {}

async fn persist_agent_lifecycle_phase(
    store: &WorkflowStore,
    lifecycle: &mut AcpRoleLifecycle,
    agent_execution_id: &str,
    role_execution_id: &str,
    phase: &'static str,
) -> Result<()> {
    let previous = lifecycle.clone();
    lifecycle.enter(phase);
    if let Err(_error) = store
        .update_running_agent_execution_lifecycle(
            agent_execution_id,
            role_execution_id,
            &lifecycle.value(),
        )
        .await
    {
        lifecycle.restore_after_persistence_failure(&previous, phase);
        return Err(RoleLifecyclePersistenceFailed.into());
    }
    Ok(())
}

async fn confirm_agent_lifecycle_phase(
    store: &WorkflowStore,
    lifecycle: &mut AcpRoleLifecycle,
    agent_execution_id: &str,
    role_execution_id: &str,
    phase: &'static str,
) -> Result<()> {
    let previous = lifecycle.clone();
    lifecycle.confirm(phase);
    if let Err(_error) = store
        .update_running_agent_execution_lifecycle(
            agent_execution_id,
            role_execution_id,
            &lifecycle.value(),
        )
        .await
    {
        lifecycle.restore_after_persistence_failure(&previous, phase);
        return Err(RoleLifecyclePersistenceFailed.into());
    }
    Ok(())
}

fn classify_supervisor_evidence(
    status: std::process::ExitStatus,
    cleanup: Result<crate::acp_process::CleanupReceiptEvidence>,
) -> SupervisorEvidence {
    use std::os::unix::process::ExitStatusExt;
    let exit_code = status.code();
    let signal = status.signal();
    let mut failure = if signal.is_some() {
        Some("SUPERVISOR_SIGNALED")
    } else if !status.success() {
        Some("SUPERVISOR_NONZERO_EXIT")
    } else {
        None
    };
    let (receipt, cleanup_confirmed) = match cleanup {
        Ok(receipt)
            if receipt.runtime == "podman"
                && receipt.expected_image_matches
                && (exit_code == Some(receipt.exit_code) || signal.is_some()) =>
        {
            (Some(receipt), true)
        }
        Ok(receipt) => {
            failure.get_or_insert("CLEANUP_RECEIPT_MISMATCH");
            (Some(receipt), false)
        }
        Err(_) => {
            failure.get_or_insert("CLEANUP_RECEIPT_UNCONFIRMED");
            (None, false)
        }
    };
    SupervisorEvidence {
        exit_code,
        signal,
        cleanup_confirmed,
        failure,
        receipt,
    }
}

#[cfg(test)]
fn validate_role_turn_completion(output: &str, evidence: &SupervisorEvidence) -> Result<()> {
    if let Some(reason) = evidence.failure.as_deref() {
        bail!("{reason}");
    }
    ensure!(evidence.cleanup_confirmed, "CLEANUP_RECEIPT_UNCONFIRMED");
    ensure!(
        output.contains(ORBIT_HANDOFF_START) && output.contains(ORBIT_HANDOFF_END),
        "HANDOFF_PARSE_FAILED"
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

fn record_supervisor_evidence(lifecycle: &mut AcpRoleLifecycle, evidence: &SupervisorEvidence) {
    lifecycle.process_exit_code = evidence.exit_code;
    lifecycle.process_signal = evidence.signal;
    lifecycle.supervisor_outcome = if evidence.signal.is_some() {
        "SIGNALED"
    } else {
        match evidence.exit_code {
            Some(0) => "EXITED_ZERO",
            Some(_) => "EXITED_NONZERO",
            None => "EXIT_UNCONFIRMED",
        }
    };
    lifecycle.supervisor_failure = evidence.failure;
    lifecycle.cleanup_state = if evidence.cleanup_confirmed {
        "CONFIRMED"
    } else {
        "UNCONFIRMED"
    };
    lifecycle.supervisor_receipt = evidence.receipt.clone();
    if evidence.exit_code.is_some() || evidence.signal.is_some() {
        lifecycle.confirm("SUPERVISOR_EXIT_OBSERVED");
    }
    if evidence.cleanup_confirmed {
        lifecycle.confirm("CLEANUP_CONFIRMED");
    } else {
        lifecycle.phase = "CLEANUP_UNCONFIRMED";
    }
}

fn preserve_primary_failure(primary: &mut Option<anyhow::Error>, secondary: anyhow::Error) {
    primary.get_or_insert(secondary);
}

fn add_supervisor_failure_if_primary_missing(
    failure: &mut Option<anyhow::Error>,
    evidence: &SupervisorEvidence,
) {
    if failure.is_some() {
        return;
    }
    if let Some(reason) = evidence.failure {
        preserve_primary_failure(failure, RoleSupervisorOutcomeFailure(reason).into());
    } else if !evidence.cleanup_confirmed {
        preserve_primary_failure(
            failure,
            RoleSupervisorOutcomeFailure("CLEANUP_RECEIPT_UNCONFIRMED").into(),
        );
    }
}

async fn terminate_supervisor_after_evidence_failure(
    child: &mut tokio::process::Child,
    request_path: &Path,
    attempt_id: &str,
    expected_image: &str,
) -> SupervisorEvidence {
    // Closing the worker lifeline asks the supervisor to stop its container
    // and write the cleanup receipt before escalation.
    drop(child.stdin.take());
    match wait_cli_supervisor(child, Duration::from_secs(5), Duration::from_secs(5)).await {
        Ok(status) => classify_supervisor_evidence(
            status,
            crate::acp_process::read_cleanup_evidence(
                request_path,
                Some(attempt_id),
                expected_image,
            ),
        ),
        Err(_) => {
            if let Some(pid) = child.id() {
                unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            }
            let _ = child.start_kill();
            let reap = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
            if let Ok(Ok(status)) = reap {
                return classify_supervisor_evidence(
                    status,
                    crate::acp_process::read_cleanup_evidence(
                        request_path,
                        Some(attempt_id),
                        expected_image,
                    ),
                );
            }
            let receipt = crate::acp_process::read_cleanup_evidence(
                request_path,
                Some(attempt_id),
                expected_image,
            )
            .ok();
            SupervisorEvidence {
                exit_code: None,
                signal: None,
                cleanup_confirmed: false,
                failure: Some("SUPERVISOR_EXIT_UNCONFIRMED"),
                receipt,
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_real_acp_turn(
    workflow_state_pool: &PgPool,
    credential_catalog_pool: &PgPool,
    wf_run: &WorkflowRun,
    role_exec: &RoleExecution,
    role: &RoleDefinition,
    target: &ResolvedExecutionTarget,
    task_text: &str,
    repo_path: &Path,
    input_handoff: Option<&HandoffArtifact>,
    cancellation: tokio::sync::watch::Receiver<bool>,
    suppress_diagnostics: bool,
) -> Result<RoleExecutionOutcome> {
    let agent_exec_id = format!("acp-exec-{}", id());
    let started_at_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as i64;
    let store = WorkflowStore::new(workflow_state_pool.clone());
    let mut lifecycle = AcpRoleLifecycle::new();
    let initial_audit = ToolCallAudit::default().metadata(0, 0, 0);
    let expected_runtime_profile =
        target
            .runtime_image_digest
            .clone()
            .or_else(|| match target.provider.as_str() {
                "codex" => Some(crate::codex_credential_enrollment::CODEX_IMAGE.to_string()),
                "antigravity" => Some(ANTIGRAVITY_IMAGE.to_string()),
                _ => None,
            });
    let initial_metadata = serde_json::json!({
        "provider": target.provider,
        "account_reference": target.credential_id,
        "credential_generation": target.credential_generation,
        "requested_model": target.requested_model,
        "resolved_model": target.resolved_model,
        "expected_runtime_identity": target.runtime_interface,
        "expected_runtime_profile": expected_runtime_profile,
        "cleanup_confirmed": false,
        "observed_model": null,
        "tool_call_audit": initial_audit,
        "lifecycle": lifecycle.value(),
    });
    store
        .start_agent_execution(
            &agent_exec_id,
            &role_exec.id,
            &format!("{}-acp", target.provider),
            Some(&target.provider),
            target.resolved_model.as_deref(),
            started_at_ms,
            target.requested_model.as_deref(),
            target.resolved_model.as_deref(),
            &initial_metadata,
        )
        .await
        .map_err(|_| anyhow::anyhow!("AGENT_EXECUTION_START_FAILED"))?;

    let result = execute_real_acp_turn_body(
        workflow_state_pool,
        credential_catalog_pool,
        wf_run,
        role_exec,
        role,
        target,
        task_text,
        repo_path,
        input_handoff,
        cancellation,
        suppress_diagnostics,
        &agent_exec_id,
        &mut lifecycle,
    )
    .await;

    if !lifecycle.needs_failure_finalization() {
        return result;
    }

    let normalized_reason = match &result {
        Err(error) => lifecycle.normalized_failure(error),
        Ok(_) => "ACP_EXECUTION_NOT_FINALIZED",
    };
    if let Err(error) = &result {
        lifecycle.record_error(error);
    } else {
        lifecycle.failed_phase = Some(lifecycle.phase);
        lifecycle.outcome = "FAILED";
        lifecycle.normalized_reason = Some(normalized_reason);
        lifecycle.phase = "TERMINAL";
    }
    lifecycle.preserve_cleanup_state_for_failure_finalization();
    let finished_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let failure_metadata = serde_json::json!({
        "cleanup_confirmed": lifecycle.cleanup_state == "CONFIRMED",
        "observed_model": null,
        "lifecycle": lifecycle.value(),
    });
    lifecycle.note_terminal_persistence_attempt();
    if store
        .finish_agent_execution(
            &agent_exec_id,
            &role_exec.id,
            finished_at_ms,
            "FAILED",
            Some(normalized_reason),
            lifecycle.process_exit_code,
            Some(normalized_reason),
            target.requested_model.as_deref(),
            target.resolved_model.as_deref(),
            None,
            0,
            0,
            0,
            0,
            &serde_json::json!({}),
            &failure_metadata,
        )
        .await
        .is_err()
    {
        lifecycle.note_terminal_persistence_failure();
        return Err(RoleTerminalPersistenceUnconfirmed.into());
    }
    Err(anyhow::anyhow!(normalized_reason))
}

#[allow(clippy::too_many_arguments)]
async fn execute_real_acp_turn_body(
    workflow_state_pool: &PgPool,
    credential_catalog_pool: &PgPool,
    wf_run: &WorkflowRun,
    role_exec: &RoleExecution,
    role: &RoleDefinition,
    target: &ResolvedExecutionTarget,
    task_text: &str,
    repo_path: &Path,
    input_handoff: Option<&HandoffArtifact>,
    mut cancellation: tokio::sync::watch::Receiver<bool>,
    suppress_diagnostics: bool,
    agent_exec_id: &str,
    lifecycle: &mut AcpRoleLifecycle,
) -> Result<RoleExecutionOutcome> {
    let store = WorkflowStore::new(workflow_state_pool.clone());

    persist_agent_lifecycle_phase(
        &store,
        lifecycle,
        agent_exec_id,
        &role_exec.id,
        "CREDENTIAL_RESOLUTION",
    )
    .await?;
    let cred_store = CredentialStore::new(credential_catalog_pool);
    let credential_ref = target
        .credential_id
        .as_deref()
        .context("CREDENTIAL_PIN_REQUIRED")?;
    let credential = cred_store
        .get(credential_ref)
        .await?
        .context("target credential not found")?;
    validate_role_credential_target(target, &credential)?;
    confirm_agent_lifecycle_phase(
        &store,
        lifecycle,
        agent_exec_id,
        &role_exec.id,
        "CREDENTIAL_RESOLVED",
    )
    .await?;

    persist_agent_lifecycle_phase(
        &store,
        lifecycle,
        agent_exec_id,
        &role_exec.id,
        "CREDENTIAL_STAGING",
    )
    .await?;
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
        let secret_bytes =
            registered_auth_diagnostic(credential_catalog_pool, &backend, &credential.reference)
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
        confirm_agent_lifecycle_phase(
            &store,
            lifecycle,
            agent_exec_id,
            &role_exec.id,
            "CREDENTIAL_STAGED",
        )
        .await?;
        persist_agent_lifecycle_phase(
            &store,
            lifecycle,
            agent_exec_id,
            &role_exec.id,
            "RUNTIME_PREPARATION",
        )
        .await?;

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
        confirm_agent_lifecycle_phase(
            &store,
            lifecycle,
            agent_exec_id,
            &role_exec.id,
            "CREDENTIAL_STAGED",
        )
        .await?;
        persist_agent_lifecycle_phase(
            &store,
            lifecycle,
            agent_exec_id,
            &role_exec.id,
            "RUNTIME_PREPARATION",
        )
        .await?;

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

    let allowed_tools = repository_tools_for_role(role);

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
    confirm_agent_lifecycle_phase(
        &store,
        lifecycle,
        agent_exec_id,
        &role_exec.id,
        "RUNTIME_PREPARED",
    )
    .await?;

    persist_agent_lifecycle_phase(
        &store,
        lifecycle,
        agent_exec_id,
        &role_exec.id,
        "SUPERVISOR_START",
    )
    .await?;
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
        .stderr(if suppress_diagnostics {
            Stdio::null()
        } else {
            Stdio::inherit()
        })
        .kill_on_drop(true);
    command.process_group(0);

    if let Some(val) = std::env::var_os("XDG_RUNTIME_DIR") {
        command.env("XDG_RUNTIME_DIR", val);
    }

    let mut child = command
        .spawn()
        .map_err(|_| anyhow::anyhow!("SUPERVISOR_START_FAILED"))?;
    lifecycle.cleanup_state = "UNCONFIRMED";
    if let Err(error) = confirm_agent_lifecycle_phase(
        &store,
        lifecycle,
        agent_exec_id,
        &role_exec.id,
        "SUPERVISOR_STARTED",
    )
    .await
    {
        let evidence = terminate_supervisor_after_evidence_failure(
            &mut child,
            &request_path,
            &req.attempt_id,
            &req.runtime.launch.image,
        )
        .await;
        record_supervisor_evidence(lifecycle, &evidence);
        return Err(error);
    }

    let Some(child_out) = child.stdout.take() else {
        let evidence = terminate_supervisor_after_evidence_failure(
            &mut child,
            &request_path,
            &req.attempt_id,
            &req.runtime.launch.image,
        )
        .await;
        record_supervisor_evidence(lifecycle, &evidence);
        return Err(anyhow::anyhow!("SUPERVISOR_STDOUT_UNAVAILABLE"));
    };
    let Some(child_in) = child.stdin.take() else {
        let evidence = terminate_supervisor_after_evidence_failure(
            &mut child,
            &request_path,
            &req.attempt_id,
            &req.runtime.launch.image,
        )
        .await;
        record_supervisor_evidence(lifecycle, &evidence);
        return Err(anyhow::anyhow!("SUPERVISOR_STDIN_UNAVAILABLE"));
    };
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
        tool_call_audit: ToolCallAudit::with_context(Some(&role.role_id), Some(&allowed_tools)),
        terminals: BTreeMap::new(),
        wf_attempt_id: Some(wf_run.attempt_id.clone()),
        role_exec_id: Some(role_exec.id.clone()),
        agent_exec_id: Some(agent_exec_id.to_string()),
        pool: Some(workflow_state_pool),
    };

    let turn = tokio::select! {
        result = async {
    persist_agent_lifecycle_phase(
        &store,
        lifecycle,
        agent_exec_id,
        &role_exec.id,
        "ACP_INITIALIZE",
    )
    .await?;
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

    confirm_agent_lifecycle_phase(
        &store,
        lifecycle,
        agent_exec_id,
        &role_exec.id,
        "ACP_INITIALIZED",
    )
    .await?;
    persist_agent_lifecycle_phase(
        &store,
        lifecycle,
        agent_exec_id,
        &role_exec.id,
        "SESSION_CREATION",
    )
    .await?;
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
    confirm_agent_lifecycle_phase(
        &store,
        lifecycle,
        agent_exec_id,
        &role_exec.id,
        "SESSION_CREATED",
    )
    .await?;

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

    persist_agent_lifecycle_phase(
        &store,
        lifecycle,
        agent_exec_id,
        &role_exec.id,
        "PROMPT_DISPATCH",
    )
    .await?;
    lifecycle.prompt_uncertainty = "IN_FLIGHT";
    persist_agent_lifecycle_phase(
        &store,
        lifecycle,
        agent_exec_id,
        &role_exec.id,
        "PROMPT_IN_FLIGHT",
    )
    .await?;
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
    lifecycle.prompt_uncertainty = "RESOLVED";
    confirm_agent_lifecycle_phase(
        &store,
        lifecycle,
        agent_exec_id,
        &role_exec.id,
        "PROMPT_RESPONSE_RECEIVED",
    )
    .await?;

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
        } => Err(anyhow::Error::new(RoleExecutionCancelled)),
        _ = tokio::time::sleep(Duration::from_secs(600)) => Err(anyhow::Error::new(RoleSupervisorTimeout)),
    };

    let failure_phase = lifecycle.phase;
    let cleanup_phase_result =
        persist_agent_lifecycle_phase(&store, lifecycle, agent_exec_id, &role_exec.id, "CLEANUP")
            .await;
    drop(wire);
    let wait_limit = if turn.is_err() { 5 } else { 60 };
    let status = wait_cli_supervisor(
        &mut child,
        Duration::from_secs(wait_limit),
        Duration::from_secs(10),
    )
    .await;
    let evidence = match status {
        Ok(status) => classify_supervisor_evidence(
            status,
            crate::acp_process::read_cleanup_evidence(
                &request_path,
                Some(&req.attempt_id),
                &req.runtime.launch.image,
            ),
        ),
        Err(_) => {
            terminate_supervisor_after_evidence_failure(
                &mut child,
                &request_path,
                &req.attempt_id,
                &req.runtime.launch.image,
            )
            .await
        }
    };
    record_supervisor_evidence(lifecycle, &evidence);
    if state.tool_calls > 0
        || !state.tool_call_audit.provider_tool_invocations.is_empty()
        || !state.tool_call_audit.unmatched_provider_updates.is_empty()
        || state.tool_call_audit.provider_tool_names_omitted > 0
    {
        lifecycle.note_tool_activity();
    }
    let mut terminal_cleanup_failed = false;
    for (_tid, term) in std::mem::take(&mut state.terminals) {
        if term.kill().await.is_err() {
            terminal_cleanup_failed = true;
        }
    }

    let finished_at_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as i64;

    let mut failure = turn.err();
    if let Err(error) = cleanup_phase_result {
        preserve_primary_failure(&mut failure, error);
    }
    if terminal_cleanup_failed {
        preserve_primary_failure(&mut failure, RoleTerminalCleanupFailed.into());
        lifecycle.cleanup_state = "UNCONFIRMED";
    }
    add_supervisor_failure_if_primary_missing(&mut failure, &evidence);
    if failure.is_none()
        && (!state.agent_output.contains(ORBIT_HANDOFF_START)
            || !state.agent_output.contains(ORBIT_HANDOFF_END))
    {
        preserve_primary_failure(&mut failure, RoleHandoffResponseMissing.into());
    }
    if let Some(error) = failure.as_ref() {
        lifecycle.phase = failure_phase;
        lifecycle.record_error(error);
    } else {
        lifecycle.finish_success();
    }
    lifecycle.cleanup_state = if evidence.cleanup_confirmed {
        if terminal_cleanup_failed {
            "UNCONFIRMED"
        } else {
            "CONFIRMED"
        }
    } else {
        "UNCONFIRMED"
    };
    lifecycle.process_exit_code = evidence.exit_code;
    lifecycle.process_signal = evidence.signal;
    lifecycle.supervisor_receipt = evidence.receipt.clone();
    lifecycle.phase = "TERMINAL";
    state.tool_call_audit.finish_interrupted_call(
        &mut state.tool_successes,
        &mut state.tool_failures,
        failure.as_ref(),
    );
    state
        .tool_call_audit
        .set_turn_completion(failure.is_none(), state.tool_calls);
    let status_text = if failure.is_some() {
        "FAILED"
    } else {
        "SUCCEEDED"
    };
    let reason = lifecycle.normalized_reason.unwrap_or("COMPLETED");
    let failure_message = failure.as_ref().map(|_| reason.to_owned());
    let model_evidence = role_model_evidence(target, None);
    let tool_call_audit =
        state
            .tool_call_audit
            .metadata(state.tool_calls, state.tool_successes, state.tool_failures);
    let tool_counts = serde_json::to_value(&state.tool_counts).map_err(|_| {
        lifecycle.note_terminal_persistence_failure();
        anyhow::Error::new(RoleTerminalPersistenceUnconfirmed)
    })?;
    lifecycle.note_terminal_persistence_attempt();
    if store
        .finish_agent_execution(
            &agent_exec_id,
            &role_exec.id,
            finished_at_ms,
            status_text,
            Some(reason),
            evidence.exit_code,
            failure_message.as_deref(),
            model_evidence.requested,
            model_evidence.configured,
            model_evidence.observed,
            i64::from(lifecycle.prompt_uncertainty != "NOT_DISPATCHED"),
            state.tool_calls as i64,
            state.tool_successes as i64,
            state.tool_failures as i64,
            &tool_counts,
            &serde_json::json!({
                "provider": target.provider,
                "cleanup_confirmed": evidence.cleanup_confirmed,
                "observed_model": null,
                "tool_call_audit": tool_call_audit,
                "lifecycle": lifecycle.value(),
            }),
        )
        .await
        .is_err()
    {
        lifecycle.note_terminal_persistence_failure();
        return Err(RoleTerminalPersistenceUnconfirmed.into());
    }
    lifecycle.note_terminal_persistence_attempt();

    if !evidence.cleanup_confirmed {
        return Err(anyhow::anyhow!(
            lifecycle
                .normalized_reason
                .unwrap_or("CLEANUP_RECEIPT_UNCONFIRMED")
        ));
    }

    if failure.is_some() {
        return Err(anyhow::anyhow!(reason));
    }

    Ok(RoleExecutionOutcome {
        raw_output: state.agent_output,
        agent_execution_ids: vec![agent_exec_id.to_string()],
        termination_reason: Some("completed".into()),
    })
}

/// Production ACP role agent executor that executes real ACP agent turns.
pub struct RealAcpRoleExecutor;

impl RealAcpRoleExecutor {
    /// Execute a real ACP role turn with separate workflow and credential stores.
    /// Production callers normally use the same pool for both stores.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_role_with_credential_catalog(
        &self,
        workflow_state_pool: &PgPool,
        credential_catalog_pool: &PgPool,
        wf_run: &WorkflowRun,
        role_exec: &RoleExecution,
        role: &RoleDefinition,
        target: &ResolvedExecutionTarget,
        task_text: &str,
        repo_path: &Path,
        input_handoff: Option<&HandoffArtifact>,
        cancellation: tokio::sync::watch::Receiver<bool>,
    ) -> Result<RoleExecutionOutcome> {
        execute_real_acp_turn(
            workflow_state_pool,
            credential_catalog_pool,
            wf_run,
            role_exec,
            role,
            target,
            task_text,
            repo_path,
            input_handoff,
            cancellation,
            true,
        )
        .await
    }
}

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
        execute_real_acp_turn(
            pool,
            pool,
            wf_run,
            role_exec,
            role,
            target,
            task_text,
            repo_path,
            input_handoff,
            cancellation,
            false,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn cleanup_receipt(exit_code: i32) -> crate::acp_process::CleanupReceiptEvidence {
        crate::acp_process::CleanupReceiptEvidence {
            format_version: 4,
            runtime: "podman",
            launch_stage: "container_wait",
            exit_code,
            expected_image_matches: true,
            diagnostic_present: false,
            diagnostic_truncated: false,
            codex_session: None,
        }
    }

    #[test]
    fn lifecycle_attempted_phases_do_not_claim_completion() {
        let cases = [
            ("CREDENTIAL_RESOLUTION", "CREDENTIAL_RESOLUTION_FAILED"),
            ("CREDENTIAL_STAGING", "CREDENTIAL_STAGING_FAILED"),
            ("RUNTIME_PREPARATION", "RUNTIME_PREPARATION_FAILED"),
            ("SUPERVISOR_START", "SUPERVISOR_START_FAILED"),
            ("ACP_INITIALIZE", "ACP_INITIALIZE_FAILED"),
            ("SESSION_CREATION", "ACP_SESSION_CREATION_FAILED"),
            ("PROMPT_IN_FLIGHT", "ACP_PROMPT_FAILED"),
        ];
        for (phase, reason) in cases {
            let mut lifecycle = AcpRoleLifecycle::new();
            lifecycle.enter(phase);
            lifecycle.record_error(&anyhow::anyhow!("synthetic stage failure"));
            assert_eq!(lifecycle.normalized_reason, Some(reason));
            assert_eq!(lifecycle.last_confirmed_phase, "AGENT_EXECUTION_CREATED");
            assert_eq!(lifecycle.attempted_phase, phase);
            assert_eq!(lifecycle.failed_phase, Some(phase));
            assert!(!lifecycle.milestones.contains(&phase));
            assert_eq!(lifecycle.phase, "TERMINAL");
        }

        let mut initialized = AcpRoleLifecycle::new();
        initialized.confirm("SUPERVISOR_STARTED");
        initialized.enter("ACP_INITIALIZE");
        initialized.record_error(&anyhow::anyhow!("initialize failed"));
        assert_eq!(initialized.last_confirmed_phase, "SUPERVISOR_STARTED");
        assert!(!initialized.milestones.contains(&"ACP_INITIALIZED"));

        let mut session = AcpRoleLifecycle::new();
        session.confirm("ACP_INITIALIZED");
        session.enter("SESSION_CREATION");
        session.record_error(&anyhow::anyhow!("session creation failed"));
        assert_eq!(session.last_confirmed_phase, "ACP_INITIALIZED");
        assert!(!session.milestones.contains(&"SESSION_CREATED"));
    }

    #[test]
    fn phase_persistence_failure_retains_attempt_and_last_confirmation() {
        let mut lifecycle = AcpRoleLifecycle::new();
        lifecycle.confirm("SUPERVISOR_STARTED");
        let previous = lifecycle.clone();
        lifecycle.confirm("ACP_INITIALIZED");
        lifecycle.restore_after_persistence_failure(&previous, "ACP_INITIALIZED");
        lifecycle.record_error(&RoleLifecyclePersistenceFailed.into());

        assert_eq!(lifecycle.failed_phase, Some("ACP_INITIALIZED"));
        assert_eq!(lifecycle.attempted_phase, "ACP_INITIALIZED");
        assert_eq!(lifecycle.phase, "TERMINAL");
        assert_eq!(lifecycle.last_confirmed_phase, "SUPERVISOR_STARTED");
        assert_eq!(lifecycle.persistence_state, "UNCONFIRMED");
        assert_eq!(
            lifecycle.normalized_reason,
            Some("AGENT_EXECUTION_LIFECYCLE_PERSISTENCE_FAILED")
        );
    }

    #[test]
    fn lifecycle_keeps_timeout_cancellation_and_persistence_outcomes_typed() {
        let mut pending = AcpRoleLifecycle::new();
        pending.enter("PROMPT_IN_FLIGHT");
        pending.prompt_uncertainty = "IN_FLIGHT";
        pending.record_error(
            &crate::acp_runtime::TurnTimeout {
                diagnostic: "pending_model_call=true private-payload-redacted".into(),
                pending_model_call: Some(true),
            }
            .into(),
        );
        assert_eq!(pending.normalized_reason, Some("ACP_PROMPT_TIMEOUT"));
        assert_eq!(pending.prompt_uncertainty, "UNRESOLVED_PENDING_MODEL_CALL");
        assert!(
            !pending
                .value()
                .to_string()
                .contains("private-payload-redacted")
        );

        let mut no_pending = AcpRoleLifecycle::new();
        no_pending.enter("PROMPT_IN_FLIGHT");
        no_pending.prompt_uncertainty = "IN_FLIGHT";
        no_pending.record_error(
            &crate::acp_runtime::TurnTimeout {
                diagnostic: "safe diagnostic".into(),
                pending_model_call: Some(false),
            }
            .into(),
        );
        assert_eq!(
            no_pending.prompt_uncertainty,
            "UNRESOLVED_NO_PENDING_MODEL_CALL"
        );

        let mut cancelled = AcpRoleLifecycle::new();
        cancelled.enter("PROMPT_IN_FLIGHT");
        cancelled.prompt_uncertainty = "IN_FLIGHT";
        cancelled.record_error(&RoleExecutionCancelled.into());
        assert_eq!(
            cancelled.normalized_reason,
            Some("ROLE_EXECUTION_CANCELLED")
        );
        assert_eq!(cancelled.prompt_uncertainty, "UNRESOLVED_UNKNOWN");

        let mut persistence = AcpRoleLifecycle::new();
        persistence.confirm("SUPERVISOR_STARTED");
        persistence.enter("ACP_INITIALIZE");
        persistence.persistence_state = "UNCONFIRMED";
        persistence.record_error(&RoleLifecyclePersistenceFailed.into());
        assert_eq!(
            persistence.normalized_reason,
            Some("AGENT_EXECUTION_LIFECYCLE_PERSISTENCE_FAILED")
        );
        assert_eq!(persistence.last_confirmed_phase, "SUPERVISOR_STARTED");
        assert_eq!(persistence.persistence_state, "UNCONFIRMED");

        let mut missing_handoff = AcpRoleLifecycle::new();
        missing_handoff.confirm("PROMPT_RESPONSE_RECEIVED");
        missing_handoff.record_error(&RoleHandoffResponseMissing.into());
        assert_eq!(
            missing_handoff.normalized_reason,
            Some("HANDOFF_RESPONSE_MISSING")
        );
        assert_eq!(
            missing_handoff.last_confirmed_phase,
            "PROMPT_RESPONSE_RECEIVED"
        );
    }

    #[test]
    fn post_spawn_failure_keeps_receipt_confirmed_cleanup() {
        use std::os::unix::process::ExitStatusExt;

        let evidence = classify_supervisor_evidence(
            std::process::ExitStatus::from_raw(0),
            Ok(cleanup_receipt(0)),
        );
        let mut lifecycle = AcpRoleLifecycle::new();
        lifecycle.cleanup_state = "UNCONFIRMED";
        record_supervisor_evidence(&mut lifecycle, &evidence);
        lifecycle.enter("ACP_INITIALIZE");
        lifecycle.record_error(&anyhow::anyhow!("initialize failed"));
        lifecycle.preserve_cleanup_state_for_failure_finalization();

        assert_eq!(lifecycle.normalized_reason, Some("ACP_INITIALIZE_FAILED"));
        assert_eq!(lifecycle.cleanup_state, "CONFIRMED");
        assert_eq!(lifecycle.supervisor_outcome, "EXITED_ZERO");
        assert_eq!(lifecycle.supervisor_failure, None);

        let mut no_runtime = AcpRoleLifecycle::new();
        no_runtime.preserve_cleanup_state_for_failure_finalization();
        assert_eq!(no_runtime.cleanup_state, "NO_RUNTIME_RESOURCE_CREATED");
    }

    #[test]
    fn cleanup_failures_preserve_primary_timeout_and_pending_model_evidence() {
        let mut lifecycle = AcpRoleLifecycle::new();
        lifecycle.enter("PROMPT_IN_FLIGHT");
        lifecycle.prompt_uncertainty = "IN_FLIGHT";
        let mut failure = Some(anyhow::Error::new(crate::acp_runtime::TurnTimeout {
            diagnostic: "pending_model_call=true redacted".into(),
            pending_model_call: Some(true),
        }));

        preserve_primary_failure(&mut failure, RoleLifecyclePersistenceFailed.into());
        lifecycle.persistence_state = "UNCONFIRMED";
        preserve_primary_failure(&mut failure, RoleTerminalCleanupFailed.into());
        lifecycle.cleanup_state = "UNCONFIRMED";
        lifecycle.record_error(failure.as_ref().expect("primary timeout retained"));

        assert_eq!(lifecycle.normalized_reason, Some("ACP_PROMPT_TIMEOUT"));
        assert_eq!(
            lifecycle.prompt_uncertainty,
            "UNRESOLVED_PENDING_MODEL_CALL"
        );
        assert_eq!(lifecycle.persistence_state, "UNCONFIRMED");
        assert_eq!(lifecycle.cleanup_state, "UNCONFIRMED");
        let encoded = lifecycle.value().to_string();
        assert!(!encoded.contains("pending_model_call=true"));
        assert!(!encoded.contains("redacted"));
    }

    #[test]
    fn unconfirmed_terminal_persistence_disables_zero_counter_fallback() {
        let mut lifecycle = AcpRoleLifecycle::new();
        assert!(lifecycle.needs_failure_finalization());
        lifecycle.note_terminal_persistence_attempt();
        lifecycle.note_terminal_persistence_failure();

        assert!(!lifecycle.needs_failure_finalization());
        assert_eq!(lifecycle.persistence_state, "UNCONFIRMED");
        assert!(
            anyhow::Error::new(RoleTerminalPersistenceUnconfirmed)
                .is::<RoleTerminalPersistenceUnconfirmed>()
        );
    }

    #[test]
    fn supervisor_outcome_survives_acp_failure_without_fabricated_tool_audit() {
        use std::os::unix::process::ExitStatusExt;

        let nonzero = classify_supervisor_evidence(
            std::process::ExitStatus::from_raw(7 << 8),
            Ok(cleanup_receipt(7)),
        );
        let mut lifecycle = AcpRoleLifecycle::new();
        lifecycle.enter("ACP_INITIALIZE");
        record_supervisor_evidence(&mut lifecycle, &nonzero);
        let mut failure = Some(anyhow::anyhow!("initialize failed"));
        add_supervisor_failure_if_primary_missing(&mut failure, &nonzero);
        lifecycle.phase = "ACP_INITIALIZE";
        lifecycle.record_error(failure.as_ref().expect("ACP failure remains primary"));

        assert_eq!(lifecycle.outcome, "FAILED");
        assert_eq!(lifecycle.normalized_reason, Some("ACP_INITIALIZE_FAILED"));
        assert_eq!(lifecycle.process_exit_code, Some(7));
        assert_eq!(lifecycle.process_signal, None);
        assert_eq!(lifecycle.supervisor_outcome, "EXITED_NONZERO");
        assert_eq!(
            lifecycle.supervisor_failure,
            Some("SUPERVISOR_NONZERO_EXIT")
        );

        let signaled = classify_supervisor_evidence(
            std::process::ExitStatus::from_raw(libc::SIGTERM),
            Ok(cleanup_receipt(0)),
        );
        let mut signaled_lifecycle = AcpRoleLifecycle::new();
        record_supervisor_evidence(&mut signaled_lifecycle, &signaled);
        let mut no_primary = None;
        add_supervisor_failure_if_primary_missing(&mut no_primary, &signaled);
        signaled_lifecycle.record_error(no_primary.as_ref().expect("signal is a failure"));
        assert_eq!(signaled_lifecycle.outcome, "FAILED");
        assert_eq!(
            signaled_lifecycle.normalized_reason,
            Some("SUPERVISOR_SIGNALED")
        );
        assert_eq!(signaled_lifecycle.process_exit_code, None);
        assert_eq!(signaled_lifecycle.process_signal, Some(libc::SIGTERM));
        assert_eq!(signaled_lifecycle.supervisor_outcome, "SIGNALED");

        for _ in 0..2 {
            let metadata = ToolCallAudit::default().metadata(0, 0, 0);
            assert_eq!(metadata["correlation_capability"], "NOT_EXERCISED");
            assert_eq!(metadata["summary"]["callback_count"], 0);
            assert_eq!(metadata["summary"]["provider_notification_count"], 0);
            assert_eq!(metadata["entries"], serde_json::json!([]));
            assert_eq!(metadata["provider_updates"], serde_json::json!([]));
        }
    }

    #[test]
    fn lifecycle_tool_and_handoff_markers_describe_observation_not_schema_acceptance() {
        let mut lifecycle = AcpRoleLifecycle::new();
        lifecycle.confirm("SESSION_CREATED");
        lifecycle.note_tool_activity();
        assert_eq!(lifecycle.tool_audit_applicability, "APPLICABLE");
        assert_eq!(lifecycle.last_confirmed_phase, "SESSION_CREATED");
        assert!(lifecycle.milestones.contains(&"TOOL_ACTIVITY_OBSERVED"));

        lifecycle.confirm("PROMPT_RESPONSE_RECEIVED");
        assert_eq!(lifecycle.last_confirmed_phase, "PROMPT_RESPONSE_RECEIVED");
        assert!(!lifecycle.milestones.contains(&"HANDOFF_SCHEMA_ACCEPTED"));
        assert_eq!(lifecycle.outcome, "IN_PROGRESS");
    }

    #[test]
    fn only_tool_call_session_updates_require_audit_persistence() {
        let tool_call = serde_json::json!({
            "method": "session/update",
            "params": { "update": { "sessionUpdate": "tool_call" } }
        });
        let agent_message = serde_json::json!({
            "method": "session/update",
            "params": { "update": { "sessionUpdate": "agent_message_chunk" } }
        });
        let unrelated_notification = serde_json::json!({
            "method": "session/update",
            "params": { "update": { "sessionUpdate": "current_mode_update" } }
        });

        assert!(is_provider_tool_call_update(&tool_call));
        assert!(!is_provider_tool_call_update(&agent_message));
        assert!(!is_provider_tool_call_update(&unrelated_notification));
    }

    #[test]
    fn audit_schema_v2_distinguishes_not_exercised_from_notification_only() {
        let empty = ToolCallAudit::default().metadata(0, 0, 0);
        assert_eq!(empty["schema_version"], 2);
        assert_eq!(empty["correlation_capability"], "NOT_EXERCISED");
        assert_eq!(empty["summary"]["callback_count"], 0);
        assert_eq!(empty["summary"]["provider_notification_count"], 0);
        let rendered = render_tool_call_audit(&serde_json::json!({"tool_call_audit": empty}));
        assert!(rendered.contains("correlation=NOT_EXERCISED"));

        let mut notification_only = ToolCallAudit::default();
        notification_only.observe_provider_tool_name(&serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "provider-unmatched",
            "title": "Read a file",
            "kind": "read",
            "status": "in_progress"
        }));
        let audit = notification_only.metadata(0, 0, 0);
        assert_eq!(audit["correlation_capability"], "PARTIAL");
        assert_eq!(audit["summary"]["provider_notification_count"], 1);
        assert_eq!(audit["summary"]["total"], 1);
    }

    #[test]
    fn provider_tool_lists_match_role_prompt_and_permissions() -> Result<()> {
        use crate::tool_surface::{CanonicalToolName as Tool, ToolMetadata};

        let read_tools = [
            "read_file",
            "list_directory",
            "find_path",
            "grep",
            "git_status",
            "git_diff",
            "git_show",
        ]
        .map(str::to_owned)
        .to_vec();
        let planner = RoleDefinition::planner_v1();
        let implementer = RoleDefinition::implementer_v1();
        let reviewer = RoleDefinition::reviewer_v1();
        let implementer_tools = [
            "read_file",
            "list_directory",
            "find_path",
            "grep",
            "git_status",
            "git_diff",
            "git_show",
            "write_file",
            "edit_file",
            "create_directory",
            "move",
            "copy",
            "delete_file",
            "delete_directory",
        ]
        .map(str::to_owned)
        .to_vec();

        for (role, expected_tools) in [
            (&planner, &read_tools),
            (&implementer, &implementer_tools),
            (&reviewer, &read_tools),
        ] {
            let tools = repository_tools_for_role(role);
            assert_eq!(&tools, expected_tools);
            assert!(
                !tools
                    .iter()
                    .any(|tool| tool == "shell" || tool.starts_with("terminal"))
            );
            assert_eq!(
                crate::coding_agent::tool_definitions(&tools)?.len(),
                tools.len()
            );

            for tool_name in &tools {
                let tool =
                    Tool::from_wire(tool_name).context("provider tool has no canonical mapping")?;
                let metadata = ToolMetadata::for_tool(tool);
                assert!(metadata.is_role_allowed(&role.role_id));
                if metadata.mutating {
                    assert_eq!(role.workspace_access, WorkspaceAccess::ReadWrite);
                    assert!(metadata.requires_mutation_lock);
                }
            }

            let prompt = build_role_prompt(
                role,
                "Inspect and update the repository",
                Path::new("/workspace"),
                "HEAD",
                None,
                None,
            );
            assert!(prompt.contains(&format!("ORBIT/ACP TOOL NAMES: {}", tools.join(", "))));
            if role.role_id == "implementer" {
                assert!(prompt.contains("Discover paths with list_directory, find_path, or grep"));
                assert!(prompt.contains("PATH_NOT_FOUND"));
            }
        }

        Ok(())
    }

    #[test]
    fn role_prompt_uses_virtual_workspace_instead_of_host_repository_path() -> Result<()> {
        let host_repository = tempdir()?;
        let host_path = host_repository.path().to_string_lossy().into_owned();

        for role in [
            RoleDefinition::planner_v1(),
            RoleDefinition::implementer_v1(),
        ] {
            let prompt = build_role_prompt(
                &role,
                "Inspect and update the repository",
                host_repository.path(),
                "HEAD",
                None,
                None,
            );

            assert!(prompt.contains(&format!(
                "Repository Workspace: {}",
                crate::acp_runtime::WORKSPACE
            )));
            assert!(prompt.contains("Repository tool paths are relative to this workspace."));
            assert!(!prompt.contains(&host_path));
        }

        Ok(())
    }

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
    async fn invocation_ids_bind_reordered_same_method_reads_to_their_results() -> Result<()> {
        use crate::acp_wire::OrbitToolInvocationMeta;

        let repo = tempdir()?;
        std::fs::write(repo.path().join("a.txt"), "result A")?;
        std::fs::write(repo.path().join("b.txt"), "result B")?;
        let (mut server, mut client) = make_test_wire();
        let mut state = AcpTurnState::new(repo.path(), WorkspaceAccess::ReadWrite);
        state.role_id = Some("implementer".into());
        state.workspace_identity = Some(repo.path().canonicalize()?.to_string_lossy().into_owned());
        let allowed = vec!["read_file".to_owned()];
        state
            .tool_call_audit
            .set_context(Some("implementer"), Some(&allowed));

        let invocation_a = OrbitToolInvocationMeta::new("oti-a", "provider-call-a")?;
        let invocation_b = OrbitToolInvocationMeta::new("oti-b", "provider-call-b")?;
        for (meta, title) in [(&invocation_a, "Read A"), (&invocation_b, "Read B")] {
            handle_acp_message(
                &mut server,
                &mut state,
                serde_json::json!({
                    "jsonrpc":"2.0",
                    "method":"session/update",
                    "params":{"update":{
                        "sessionUpdate":"tool_call",
                        "toolCallId":meta.provider_tool_call_id,
                        "title":title,
                        "kind":"read",
                        "status":"in_progress"
                    }},
                    "_meta":meta.envelope_metadata()
                }),
            )
            .await?;
        }

        // Reverse callback order. Both callbacks use the same provider method,
        // so FIFO or method/name matching would bind these results incorrectly.
        for (rpc_id, path, meta, expected) in [
            ("rpc-b", "b.txt", &invocation_b, "result B"),
            ("rpc-a", "a.txt", &invocation_a, "result A"),
        ] {
            handle_acp_message(
                &mut server,
                &mut state,
                serde_json::json!({
                    "jsonrpc":"2.0",
                    "id":rpc_id,
                    "method":"fs/read_text_file",
                    "params":{"path":path},
                    "_meta":meta.envelope_metadata()
                }),
            )
            .await?;
            let response = client.read().await?;
            assert_eq!(response["id"], rpc_id);
            assert_eq!(response["result"]["content"], expected);
        }

        state
            .tool_call_audit
            .set_turn_completion(true, state.tool_calls);
        let audit = state.tool_call_audit.metadata(
            state.tool_calls,
            state.tool_successes,
            state.tool_failures,
        );
        assert_eq!(audit["correlation_capability"], "SUPPORTED");
        assert_eq!(audit["summary"]["unmatched_provider_calls"], 0);
        assert_eq!(audit["summary"]["unmatched_callbacks"], 0);
        assert_eq!(audit["entries"][0]["tool_invocation_id"], "oti-b");
        assert_eq!(
            audit["entries"][0]["provider_tool_call_id"],
            "provider-call-b"
        );
        assert_eq!(audit["entries"][0]["callback_request_id"], "s:rpc-b");
        assert_eq!(audit["entries"][1]["tool_invocation_id"], "oti-a");
        assert_eq!(
            audit["entries"][1]["provider_tool_call_id"],
            "provider-call-a"
        );
        assert_eq!(audit["entries"][1]["callback_request_id"], "s:rpc-a");
        assert!(audit["entries"].as_array().unwrap().iter().all(|entry| {
            entry["provider_update_correlation"] == "CORRELATED"
                && entry["terminal_state"] == "SUCCESS"
        }));
        Ok(())
    }

    #[tokio::test]
    async fn correlated_path_not_found_is_a_terminal_failure_with_safe_ids() -> Result<()> {
        use crate::acp_wire::OrbitToolInvocationMeta;

        let repo = tempdir()?;
        let (mut server, mut client) = make_test_wire();
        let mut state = AcpTurnState::new(repo.path(), WorkspaceAccess::ReadWrite);
        state.role_id = Some("implementer".into());
        state.workspace_identity = Some(repo.path().canonicalize()?.to_string_lossy().into_owned());
        let allowed = vec!["read_file".to_owned()];
        state
            .tool_call_audit
            .set_context(Some("implementer"), Some(&allowed));
        let invocation = OrbitToolInvocationMeta::new("oti-missing", "provider-call-missing")?;
        handle_acp_message(
            &mut server,
            &mut state,
            serde_json::json!({
                "method":"session/update",
                "params":{"update":{
                    "sessionUpdate":"tool_call",
                    "toolCallId":"provider-call-missing",
                    "title":"Read absent file",
                    "kind":"read",
                    "status":"in_progress"
                }},
                "_meta":invocation.envelope_metadata()
            }),
        )
        .await?;
        handle_acp_message(
            &mut server,
            &mut state,
            serde_json::json!({
                "id":"rpc-missing",
                "method":"fs/read_text_file",
                "params":{"path":"absent.txt"},
                "_meta":invocation.envelope_metadata()
            }),
        )
        .await?;
        let _ = client.read().await?;
        state
            .tool_call_audit
            .set_turn_completion(true, state.tool_calls);
        let audit = state.tool_call_audit.metadata(
            state.tool_calls,
            state.tool_successes,
            state.tool_failures,
        );
        assert_eq!(
            audit["entries"][0]["provider_update_correlation"],
            "CORRELATED"
        );
        assert_eq!(audit["entries"][0]["terminal_state"], "FAILED");
        assert_eq!(
            audit["entries"][0]["error_code"],
            crate::tool_surface::ERR_PATH_NOT_FOUND
        );
        assert_eq!(audit["entries"][0]["tool_invocation_id"], "oti-missing");
        assert_eq!(audit["entries"][0]["callback_request_id"], "s:rpc-missing");
        Ok(())
    }

    #[tokio::test]
    async fn correlated_read_with_closed_response_peer_is_failed_not_successful() -> Result<()> {
        use crate::acp_wire::OrbitToolInvocationMeta;

        let repo = tempdir()?;
        std::fs::write(repo.path().join("candidate.txt"), "known candidate")?;
        let (mut server, peer) = make_test_wire();
        drop(peer);
        let mut state = AcpTurnState::new(repo.path(), WorkspaceAccess::ReadOnly);
        state.role_id = Some("planner".into());
        state.workspace_identity = Some(repo.path().canonicalize()?.to_string_lossy().into_owned());
        let allowed = vec!["read_file".to_owned()];
        state
            .tool_call_audit
            .set_context(Some("planner"), Some(&allowed));
        let invocation = OrbitToolInvocationMeta::new("oti-response-fail", "provider-read")?;

        handle_acp_message(
            &mut server,
            &mut state,
            serde_json::json!({
                "method":"session/update",
                "params":{"update":{
                    "sessionUpdate":"tool_call",
                    "toolCallId":"provider-read",
                    "title":"Read candidate",
                    "kind":"read",
                    "status":"in_progress"
                }},
                "_meta":invocation.envelope_metadata()
            }),
        )
        .await?;

        let response = handle_acp_message(
            &mut server,
            &mut state,
            serde_json::json!({
                "jsonrpc":"2.0",
                "id":"rpc-read",
                "method":"fs/read_text_file",
                "params":{"path":"candidate.txt"},
                "_meta":invocation.envelope_metadata()
            }),
        )
        .await;
        assert!(
            response.is_err(),
            "closed response peer must fail the handler"
        );
        assert_eq!(state.tool_calls, 1);
        assert_eq!(state.tool_successes, 0);
        assert_eq!(state.tool_failures, 1);
        assert_eq!(state.tool_call_audit.entries.len(), 1);
        assert_eq!(
            state.tool_call_audit.entries[0].provider_update_correlation,
            "CORRELATED"
        );
        assert_eq!(
            state.tool_call_audit.entries[0].outcome,
            Some(ToolCallOutcome::ExecutionFailure)
        );
        assert_eq!(state.tool_call_audit.entries[0].terminal_state, "FAILED");
        assert_eq!(
            state.tool_call_audit.entries[0].error_code,
            Some("TOOL_RESPONSE_FAILED")
        );
        Ok(())
    }

    #[test]
    fn failed_mutating_callback_keeps_effect_state_uncertain() -> Result<()> {
        use crate::tool_surface::CanonicalToolName as Tool;

        let mut audit = ToolCallAudit::default();
        let call = audit.begin_call(1, "fs/write_text_file", Some(Tool::FsWriteTextFile), 0, 0);
        audit.finish_call(
            call,
            ToolCallOutcome::ExecutionFailure,
            Some("TOOL_RESPONSE_FAILED"),
        );
        assert_eq!(audit.entries[0].terminal_state, "FAILED");
        assert_eq!(
            audit.entries[0].outcome,
            Some(ToolCallOutcome::ExecutionFailure)
        );
        assert_eq!(audit.entries[0].error_code, Some("TOOL_RESPONSE_FAILED"));
        assert_eq!(audit.entries[0].mutation_applied, None);
        Ok(())
    }

    #[test]
    fn unmatched_duplicate_and_pending_invocations_never_authorize_mutation() -> Result<()> {
        use crate::{acp_wire::OrbitToolInvocationMeta, tool_surface::CanonicalToolName as Tool};

        let invocation = OrbitToolInvocationMeta::new("oti-write", "provider-write")?;
        let update = serde_json::json!({
            "sessionUpdate":"tool_call",
            "toolCallId":"provider-write",
            "title":"private provider display text",
            "kind":"edit",
            "status":"in_progress"
        });
        let callback_id = serde_json::json!("rpc-write");

        let mut missing_update = ToolCallAudit::default();
        let missing_call = missing_update.begin_call_with_context(
            1,
            "fs/write_text_file",
            Some(&callback_id),
            Ok(Some(invocation.clone())),
            Some(Tool::FsWriteTextFile),
            0,
            0,
        );
        assert!(!mutation_correlation_permits_dispatch(
            &missing_update,
            missing_call,
            true
        ));
        missing_update.finish_call(
            missing_call,
            ToolCallOutcome::ExecutionFailure,
            Some("PROVIDER_CALLBACK_UNRESOLVED"),
        );
        missing_update.set_turn_completion(false, 1);
        assert_eq!(
            missing_update.entries[0].provider_update_correlation,
            "CALLBACK_WITHOUT_UPDATE"
        );
        assert_eq!(missing_update.entries[0].terminal_state, "FAILED");

        let mut notification_only = ToolCallAudit::default();
        notification_only
            .observe_provider_tool_name_with_metadata(&update, Ok(Some(invocation.clone())));
        notification_only.set_turn_completion(false, 0);
        let notification_audit = notification_only.metadata(0, 0, 0);
        assert_eq!(notification_audit["summary"]["unmatched_provider_calls"], 1);
        assert_eq!(notification_audit["correlation_capability"], "PARTIAL");
        assert_eq!(
            notification_audit["entries"][0]["terminal_state"],
            "UNRESOLVED"
        );

        let mut duplicate_update = ToolCallAudit::default();
        duplicate_update
            .observe_provider_tool_name_with_metadata(&update, Ok(Some(invocation.clone())));
        duplicate_update
            .observe_provider_tool_name_with_metadata(&update, Ok(Some(invocation.clone())));
        let duplicate_call = duplicate_update.begin_call_with_context(
            1,
            "fs/write_text_file",
            Some(&callback_id),
            Ok(Some(invocation.clone())),
            Some(Tool::FsWriteTextFile),
            0,
            0,
        );
        assert_eq!(
            duplicate_update.entries[0].provider_update_correlation,
            "INVALIDATED_INVOCATION"
        );
        assert!(!mutation_correlation_permits_dispatch(
            &duplicate_update,
            duplicate_call,
            true
        ));

        let mut mismatched_provider_id = ToolCallAudit::default();
        let mismatched_update = serde_json::json!({
            "sessionUpdate":"tool_call",
            "toolCallId":"different-provider-id",
            "title":"Read display title",
            "kind":"edit",
            "status":"in_progress"
        });
        mismatched_provider_id.observe_provider_tool_name_with_metadata(
            &mismatched_update,
            Ok(Some(invocation.clone())),
        );
        let mismatched_call = mismatched_provider_id.begin_call_with_context(
            1,
            "fs/write_text_file",
            Some(&callback_id),
            Ok(Some(invocation.clone())),
            Some(Tool::FsWriteTextFile),
            0,
            0,
        );
        assert!(!mutation_correlation_permits_dispatch(
            &mismatched_provider_id,
            mismatched_call,
            true
        ));
        assert_eq!(
            mismatched_provider_id.unmatched_provider_updates[0].correlation_state,
            "PROVIDER_ID_MISMATCH"
        );

        let mut duplicate_callback = ToolCallAudit::default();
        duplicate_callback
            .observe_provider_tool_name_with_metadata(&update, Ok(Some(invocation.clone())));
        let first = duplicate_callback.begin_call_with_context(
            1,
            "fs/write_text_file",
            Some(&callback_id),
            Ok(Some(invocation.clone())),
            Some(Tool::FsWriteTextFile),
            0,
            0,
        );
        assert!(mutation_correlation_permits_dispatch(
            &duplicate_callback,
            first,
            true
        ));
        duplicate_callback.finish_call(first, ToolCallOutcome::Success, None);
        let replay = duplicate_callback.begin_call_with_context(
            2,
            "fs/write_text_file",
            Some(&callback_id),
            Ok(Some(invocation.clone())),
            Some(Tool::FsWriteTextFile),
            1,
            0,
        );
        assert_eq!(
            duplicate_callback.entries[1].provider_update_correlation,
            "DUPLICATE_CALLBACK_ID"
        );
        assert!(!mutation_correlation_permits_dispatch(
            &duplicate_callback,
            replay,
            true
        ));

        let mut invocation_replay = ToolCallAudit::default();
        invocation_replay
            .observe_provider_tool_name_with_metadata(&update, Ok(Some(invocation.clone())));
        let first_effect = invocation_replay.begin_call_with_context(
            1,
            "fs/write_text_file",
            Some(&serde_json::json!("rpc-write-first")),
            Ok(Some(invocation.clone())),
            Some(Tool::FsWriteTextFile),
            0,
            0,
        );
        assert!(mutation_correlation_permits_dispatch(
            &invocation_replay,
            first_effect,
            true
        ));
        invocation_replay.finish_call(first_effect, ToolCallOutcome::Success, None);

        let replayed_effect = invocation_replay.begin_call_with_context(
            2,
            "fs/write_text_file",
            Some(&serde_json::json!("rpc-write-second")),
            Ok(Some(invocation.clone())),
            Some(Tool::FsWriteTextFile),
            1,
            0,
        );
        assert_eq!(
            invocation_replay.entries[1].provider_update_correlation,
            "DUPLICATE_INVOCATION_CALLBACK"
        );
        let mut mutation_effect_count = 1;
        if mutation_correlation_permits_dispatch(&invocation_replay, replayed_effect, true) {
            mutation_effect_count += 1;
        } else {
            invocation_replay.finish_call(
                replayed_effect,
                ToolCallOutcome::ExecutionFailure,
                Some("PROVIDER_CALLBACK_UNRESOLVED"),
            );
        }
        assert_eq!(mutation_effect_count, 1);
        assert_eq!(invocation_replay.entries[1].terminal_state, "FAILED");
        assert_eq!(invocation_replay.unmatched_callback_count, 1);

        let mut callback_tracking_full = ToolCallAudit::default();
        callback_tracking_full
            .observe_provider_tool_name_with_metadata(&update, Ok(Some(invocation.clone())));
        callback_tracking_full
            .callback_request_ids
            .extend((0..TOOL_CALL_AUDIT_LIMIT).map(|index| format!("s:rpc-{index}")));
        let overflow_call = callback_tracking_full.begin_call_with_context(
            1,
            "fs/write_text_file",
            Some(&serde_json::json!("rpc-after-limit")),
            Ok(Some(invocation.clone())),
            Some(Tool::FsWriteTextFile),
            0,
            0,
        );
        assert_eq!(
            callback_tracking_full.entries[0].provider_update_correlation,
            "CALLBACK_TRACKING_LIMIT_EXCEEDED"
        );
        assert_eq!(
            callback_tracking_full.callback_request_ids.len(),
            TOOL_CALL_AUDIT_LIMIT
        );
        assert!(!mutation_correlation_permits_dispatch(
            &callback_tracking_full,
            overflow_call,
            true
        ));

        let mut timed_out = ToolCallAudit::default();
        timed_out.observe_provider_tool_name_with_metadata(&update, Ok(Some(invocation.clone())));
        let pending = timed_out.begin_call_with_context(
            1,
            "fs/write_text_file",
            Some(&callback_id),
            Ok(Some(invocation.clone())),
            Some(Tool::FsWriteTextFile),
            0,
            0,
        );
        assert!(mutation_correlation_permits_dispatch(
            &timed_out, pending, true
        ));
        let mut successes = 0;
        let mut failures = 0;
        timed_out.finish_interrupted_call(
            &mut successes,
            &mut failures,
            Some(&anyhow::anyhow!("ROLE_SUPERVISOR_TIMEOUT")),
        );
        timed_out.set_turn_completion(false, 1);
        assert_eq!(timed_out.entries[0].terminal_state, "TIMED_OUT");
        assert_eq!(successes, 0);
        assert_eq!(failures, 1);

        let mut exited = ToolCallAudit::default();
        exited.observe_provider_tool_name_with_metadata(&update, Ok(Some(invocation.clone())));
        let pending_at_exit = exited.begin_call_with_context(
            1,
            "fs/write_text_file",
            Some(&callback_id),
            Ok(Some(invocation.clone())),
            Some(Tool::FsWriteTextFile),
            0,
            0,
        );
        assert!(mutation_correlation_permits_dispatch(
            &exited,
            pending_at_exit,
            true
        ));
        exited.set_turn_completion(false, 1);
        assert_eq!(exited.entries[0].terminal_state, "PROCESS_EXIT_UNRESOLVED");
        assert_eq!(
            exited.entries[0].outcome,
            Some(ToolCallOutcome::ExecutionFailure)
        );

        let encoded = serde_json::to_string(&notification_audit)?;
        assert!(!encoded.contains("private provider display text"));
        Ok(())
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

    #[tokio::test]
    async fn tool_call_audit_records_safe_read_failures_and_provider_mapping() -> Result<()> {
        let repo = tempdir()?;
        let path = repo.path();
        let marker = "synthetic-provider-payload-marker";
        std::fs::write(path.join("fixture.txt"), marker)?;
        let (mut server, mut client) = make_test_wire();
        let allowed = vec!["read_file".to_string()];
        let mut state = AcpTurnState::new(path, WorkspaceAccess::ReadWrite);
        state.role_id = Some("implementer".into());
        state.workspace_identity = Some(path.canonicalize()?.to_string_lossy().into_owned());
        state
            .tool_call_audit
            .set_context(Some("implementer"), Some(&allowed));

        for (id, file) in [(1, "fixture.txt"), (2, "missing-private-name.txt")] {
            handle_acp_message(
                &mut server,
                &mut state,
                serde_json::json!({
                    "method": "session/update",
                    "params": {"update": {"sessionUpdate": "tool_call", "title": "orbit_read_file"}}
                }),
            )
            .await?;
            handle_acp_message(
                &mut server,
                &mut state,
                serde_json::json!({
                    "id": id,
                    "method": "fs/read_text_file",
                    "params": {"path": file}
                }),
            )
            .await?;
            let _ = client.read().await?;
        }
        state
            .tool_call_audit
            .set_turn_completion(true, state.tool_calls);
        let audit = state.tool_call_audit.metadata(
            state.tool_calls,
            state.tool_successes,
            state.tool_failures,
        );
        let entries = audit["entries"].as_array().unwrap();
        assert_eq!(entries[0]["provider_tool_name"], "fs/read_text_file");
        assert_eq!(entries[0]["provider_name_mapping"], "MATCH");
        assert_eq!(entries[0]["advertised_to_provider"], true);
        assert_eq!(entries[0]["role_allowed"], true);
        assert_eq!(entries[0]["mutation_applied"], false);
        assert_eq!(entries[0]["later_callback_observed"], true);
        assert_eq!(entries[1]["outcome"], "EXECUTION_FAILURE");
        assert_eq!(entries[1]["error_code"], "PATH_NOT_FOUND");
        assert_eq!(
            entries[0]["path_arguments"][0]["state"],
            "WORKSPACE_RELATIVE"
        );
        assert_eq!(
            entries[0]["path_arguments"][0]["workspace_relative_path"],
            "fixture.txt"
        );
        assert_eq!(entries[0]["path_arguments"][0]["exists"], true);
        assert_eq!(
            entries[1]["path_arguments"][0]["state"],
            "WORKSPACE_RELATIVE"
        );
        assert_eq!(
            entries[1]["path_arguments"][0]["workspace_relative_path"],
            "missing-private-name.txt"
        );
        assert_eq!(entries[1]["path_arguments"][0]["exists"], false);
        let encoded = audit.to_string();
        let report = render_tool_call_audit(&serde_json::json!({"tool_call_audit": audit}));
        assert!(!encoded.contains(marker));
        assert!(!report.contains(marker));
        for safe_path in ["fixture.txt", "missing-private-name.txt"] {
            assert!(report.contains(safe_path));
        }
        assert!(report.contains("WORKSPACE_RELATIVE(fixture.txt; exists=true)"));
        assert!(report.contains("WORKSPACE_RELATIVE(missing-private-name.txt; exists=false)"));

        for private_value in [marker, "synthetic-provider-payload-marker"] {
            assert!(!encoded.contains(private_value));
            assert!(!report.contains(private_value));
        }
        assert!(report.contains("PATH_NOT_FOUND"));
        Ok(())
    }

    #[test]
    fn tool_call_audit_redacts_host_paths_and_invalid_path_arguments() -> Result<()> {
        use crate::tool_surface::CanonicalToolName as Tool;

        let repo = tempdir()?;
        let outside = tempdir()?;
        let host_path = outside.path().join("private-host-secret.txt");
        std::fs::write(&host_path, "synthetic")?;
        std::os::unix::fs::symlink(outside.path(), repo.path().join("outside-link"))?;
        let allowed = vec!["read_file".to_string()];
        let mut audit = ToolCallAudit::with_context(Some("implementer"), Some(&allowed));
        let cases = [
            serde_json::json!({"path": host_path.to_string_lossy()}),
            serde_json::json!({"path": "../private-host-secret.txt"}),
            serde_json::json!({"path": 17}),
            serde_json::json!({"path": "outside-link/private-host-secret.txt"}),
        ];
        for (index, params) in cases.iter().enumerate() {
            let sequence = index as u64 + 1;
            audit.begin_call(
                sequence,
                "fs/read_text_file",
                Some(Tool::FsReadTextFile),
                0,
                sequence - 1,
            );
            audit.record_path_arguments(sequence, Tool::FsReadTextFile, params, repo.path());
            audit.finish_call(
                sequence,
                ToolCallOutcome::InvalidRequest,
                Some("INVALID_REQUEST"),
            );
        }

        assert_eq!(
            audit.entries[0].path_arguments[0].state,
            PathAuditState::OutsideWorkspaceRedacted
        );
        assert_eq!(
            audit.entries[0].path_arguments[0].workspace_relative_path,
            None
        );
        assert_eq!(
            audit.entries[1].path_arguments[0].state,
            PathAuditState::InvalidPath
        );
        assert_eq!(
            audit.entries[2].path_arguments[0].state,
            PathAuditState::InvalidPath
        );
        assert_eq!(
            audit.entries[3].path_arguments[0].state,
            PathAuditState::OutsideWorkspaceRedacted
        );
        assert_eq!(
            audit.entries[3].path_arguments[0].workspace_relative_path,
            None
        );

        let metadata = serde_json::json!({
            "tool_call_audit": audit.metadata(4, 0, 4)
        });
        let encoded = metadata.to_string();
        let report = render_tool_call_audit(&metadata);
        assert!(!encoded.contains(&host_path.to_string_lossy().to_string()));
        assert!(!report.contains(&host_path.to_string_lossy().to_string()));
        assert!(!encoded.contains("private-host-secret.txt"));
        assert!(!report.contains("private-host-secret.txt"));
        assert!(report.contains("OUTSIDE_WORKSPACE_REDACTED"));
        assert!(report.contains("INVALID_PATH"));
        Ok(())
    }

    #[test]
    fn tool_call_audit_normalizes_and_bounds_workspace_paths() -> Result<()> {
        use crate::tool_surface::CanonicalToolName as Tool;

        let repo = tempdir()?;
        std::fs::write(repo.path().join("virtual.txt"), "virtual")?;
        std::fs::write(repo.path().join("absolute.txt"), "absolute")?;
        let canonical_repo = repo.path().canonicalize()?;
        let long_relative_path = format!("{}.txt", "x".repeat(TOOL_PATH_DISPLAY_LIMIT + 32));
        let over_limit_path = "s".repeat(TOOL_PATH_INPUT_LIMIT + 1);
        let cases = [
            serde_json::json!({"path": "/orbit/home/workspace/virtual.txt"}),
            serde_json::json!({"path": canonical_repo.join("absolute.txt").to_string_lossy()}),
            serde_json::json!({"path": long_relative_path.clone()}),
            serde_json::json!({"path": over_limit_path.clone()}),
        ];
        let allowed = vec!["read_file".to_string()];
        let mut audit = ToolCallAudit::with_context(Some("implementer"), Some(&allowed));
        for (index, params) in cases.iter().enumerate() {
            let sequence = index as u64 + 1;
            audit.begin_call(
                sequence,
                "fs/read_text_file",
                Some(Tool::FsReadTextFile),
                0,
                sequence - 1,
            );
            audit.record_path_arguments(sequence, Tool::FsReadTextFile, params, repo.path());
            audit.finish_call(sequence, ToolCallOutcome::Success, None);
        }

        assert_eq!(
            audit.entries[0].path_arguments[0].state,
            PathAuditState::WorkspaceRelative
        );
        assert_eq!(
            audit.entries[0].path_arguments[0]
                .workspace_relative_path
                .as_deref(),
            Some("virtual.txt")
        );
        assert_eq!(audit.entries[0].path_arguments[0].exists, Some(true));
        assert_eq!(
            audit.entries[1].path_arguments[0].state,
            PathAuditState::WorkspaceRelative
        );
        assert_eq!(
            audit.entries[1].path_arguments[0]
                .workspace_relative_path
                .as_deref(),
            Some("absolute.txt")
        );
        assert_eq!(audit.entries[1].path_arguments[0].exists, Some(true));

        let long_path = &audit.entries[2].path_arguments[0];
        assert_eq!(long_path.state, PathAuditState::WorkspaceRelative);
        assert!(long_path.display_truncated);
        let stored_path = long_path.workspace_relative_path.as_deref().unwrap();
        assert_eq!(stored_path.len(), TOOL_PATH_DISPLAY_LIMIT);
        assert!(stored_path.ends_with("..."));
        let rendered_path =
            render_path_arguments(&serde_json::to_value(&audit.entries[2].path_arguments)?);
        let rendered_start = rendered_path
            .find("WORKSPACE_RELATIVE(")
            .context("rendered path state is missing")?
            + "WORKSPACE_RELATIVE(".len();
        let rendered_end = rendered_path
            .find("; exists=")
            .context("rendered path existence marker is missing")?;
        let rendered_relative_path = &rendered_path[rendered_start..rendered_end];
        assert_eq!(rendered_relative_path, stored_path);
        assert!(rendered_relative_path.len() <= TOOL_PATH_DISPLAY_LIMIT);

        assert_eq!(
            audit.entries[3].path_arguments[0].state,
            PathAuditState::InvalidPath
        );
        assert_eq!(
            audit.entries[3].path_arguments[0].workspace_relative_path,
            None
        );
        let metadata = serde_json::json!({
            "tool_call_audit": audit.metadata(4, 4, 0)
        });
        let encoded = metadata.to_string();
        let report = render_tool_call_audit(&metadata);
        assert!(report.contains("WORKSPACE_RELATIVE(virtual.txt; exists=true)"));
        assert!(report.contains("WORKSPACE_RELATIVE(absolute.txt; exists=true)"));
        assert!(report.contains("INVALID_PATH"));
        assert!(!encoded.contains(&canonical_repo.to_string_lossy().to_string()));
        assert!(!report.contains(&canonical_repo.to_string_lossy().to_string()));
        assert!(!encoded.contains(&over_limit_path));
        assert!(!report.contains(&over_limit_path));
        Ok(())
    }

    #[test]
    fn git_path_filter_audit_records_only_safe_workspace_paths() -> Result<()> {
        use crate::tool_surface::CanonicalToolName as Tool;

        let repo = tempdir()?;
        let virtual_filter = format!("{}/private-filter.txt", crate::acp_runtime::WORKSPACE);
        let relative_filter = "./private-filter.txt";
        let absolute_filter = repo
            .path()
            .join("private filter name.txt")
            .to_string_lossy()
            .into_owned();
        let outside_filter = "/private/orbit-audit-secret/provider-token";
        let traversal_filter = "../orbit-audit-secret/provider-token";
        let mut audit = ToolCallAudit::default();
        let cases = [
            (
                1,
                Tool::GitStatus,
                serde_json::json!({"path": virtual_filter}),
            ),
            (
                2,
                Tool::GitStatus,
                serde_json::json!({"path": relative_filter}),
            ),
            (
                3,
                Tool::GitDiff,
                serde_json::json!({"path": absolute_filter.clone()}),
            ),
            (
                4,
                Tool::GitStatus,
                serde_json::json!({"path": outside_filter}),
            ),
            (
                5,
                Tool::GitDiff,
                serde_json::json!({"path": traversal_filter}),
            ),
            (6, Tool::GitStatus, serde_json::json!({"path": 987654321})),
            (7, Tool::GitDiff, serde_json::json!({})),
            (8, Tool::GitDiff, serde_json::json!({"path": null})),
            (9, Tool::FsListDirectory, serde_json::json!({"path": null})),
            (10, Tool::FsFindPath, serde_json::json!({"path": null})),
            (11, Tool::GitShow, serde_json::json!({"path": null})),
        ];
        for (sequence, tool, params) in cases {
            audit.begin_call(sequence, tool.as_str(), Some(tool), sequence - 1, 0);
            audit.record_path_arguments(sequence, tool, &params, repo.path());
            audit.finish_call(sequence, ToolCallOutcome::Success, None);
        }

        for entry in audit.entries.iter().take(3) {
            assert_eq!(
                entry.path_arguments[0].state,
                PathAuditState::WorkspaceRelative
            );
        }
        assert_eq!(
            audit.entries[0].path_arguments[0]
                .workspace_relative_path
                .as_deref(),
            Some("private-filter.txt")
        );
        assert_eq!(audit.entries[0].path_arguments[0].exists, Some(false));
        assert_eq!(
            audit.entries[1].path_arguments[0]
                .workspace_relative_path
                .as_deref(),
            Some("private-filter.txt")
        );
        assert_eq!(
            audit.entries[2].path_arguments[0]
                .workspace_relative_path
                .as_deref(),
            Some("private%20filter%20name.txt")
        );
        assert_eq!(
            audit.entries[3].path_arguments[0].state,
            PathAuditState::OutsideWorkspaceRedacted
        );
        assert_eq!(
            audit.entries[3].path_arguments[0].workspace_relative_path,
            None
        );
        assert_eq!(
            audit.entries[4].path_arguments[0].state,
            PathAuditState::InvalidPath
        );
        assert_eq!(
            audit.entries[5].path_arguments[0].state,
            PathAuditState::InvalidPath
        );
        assert_eq!(
            audit.entries[5].path_arguments[0].workspace_relative_path,
            None
        );
        for entry in audit.entries.iter().skip(6) {
            assert!(entry.path_arguments.is_empty());
        }
        let metadata = serde_json::json!({
            "tool_call_audit": audit.metadata(11, 11, 0)
        });
        let encoded = metadata.to_string();
        let report = render_tool_call_audit(&metadata);
        assert!(!encoded.contains(&virtual_filter));
        assert!(!report.contains(&virtual_filter));
        assert!(!encoded.contains(relative_filter));
        assert!(!report.contains(relative_filter));
        assert!(!encoded.contains(&absolute_filter));
        assert!(!report.contains(&absolute_filter));
        assert!(!encoded.contains(outside_filter));
        assert!(!report.contains(outside_filter));
        assert!(!encoded.contains(traversal_filter));
        assert!(!report.contains(traversal_filter));
        assert!(report.contains("WORKSPACE_RELATIVE(private-filter.txt; exists=false)"));
        assert!(report.contains("private%20filter%20name.txt"));
        assert!(report.contains("path=OUTSIDE_WORKSPACE_REDACTED"));
        assert!(report.contains("path=INVALID_PATH"));
        assert!(!encoded.contains("private filter name.txt"));
        assert!(!report.contains("private filter name.txt"));
        assert!(!encoded.contains("987654321"));
        assert!(!report.contains("987654321"));
        let absent_filter_row = report.lines().find(|line| line.starts_with("7 |"));
        assert!(absent_filter_row.is_some_and(|line| line.contains("|  | Tool call completed.")));
        for sequence in 8..=11 {
            let row = report
                .lines()
                .find(|line| line.starts_with(&format!("{sequence} |")));
            assert!(row.is_some_and(|line| line.contains("|  | Tool call completed.")));
        }
        Ok(())
    }

    #[tokio::test]
    async fn empty_grep_query_is_recorded_as_invalid_request() -> Result<()> {
        let repo = tempdir()?;
        let (mut server, mut client) = make_test_wire();
        let mut state = AcpTurnState::new(repo.path(), WorkspaceAccess::ReadWrite);
        state.role_id = Some("implementer".into());
        state.workspace_identity = Some(repo.path().canonicalize()?.to_string_lossy().into_owned());

        handle_acp_message(
            &mut server,
            &mut state,
            serde_json::json!({
                "id": 1,
                "method": "search/grep",
                "params": {"query": ""}
            }),
        )
        .await?;
        let _ = client.read().await?;
        state
            .tool_call_audit
            .set_turn_completion(true, state.tool_calls);
        let audit = state.tool_call_audit.metadata(
            state.tool_calls,
            state.tool_successes,
            state.tool_failures,
        );
        assert_eq!(audit["entries"][0]["outcome"], "INVALID_REQUEST");
        assert_eq!(audit["entries"][0]["error_code"], "INVALID_REQUEST");
        Ok(())
    }

    #[test]
    fn tool_call_audit_keeps_typed_failure_outcomes() {
        let mut audit = ToolCallAudit::default();
        let outcomes = [
            (ToolCallOutcome::ExpectedDenial, "READ_ONLY_ROLE"),
            (ToolCallOutcome::InvalidRequest, "INVALID_REQUEST"),
            (ToolCallOutcome::ExecutionFailure, "TOOL_EXECUTION_FAILED"),
            (ToolCallOutcome::Unsupported, "UNSUPPORTED_TOOL"),
        ];
        for (index, (outcome, code)) in outcomes.into_iter().enumerate() {
            let sequence = index as u64 + 1;
            let call = audit.begin_call(sequence, "unsupported", None, 0, sequence - 1);
            audit.finish_call(call, outcome, Some(code));
            assert_eq!(audit.entries[index].outcome, Some(outcome));
            assert_eq!(audit.entries[index].error_code, Some(code));
        }
        for (sequence, failure, expected) in [
            (5, "ROLE_SUPERVISOR_TIMEOUT", ToolCallOutcome::Timeout),
            (6, "ROLE_EXECUTION_CANCELLED", ToolCallOutcome::Cancelled),
        ] {
            audit.begin_call(sequence, "unsupported", None, 0, sequence - 1);
            let mut successes = 0;
            let mut failures = sequence - 1;
            audit.finish_interrupted_call(
                &mut successes,
                &mut failures,
                Some(&anyhow::anyhow!(failure)),
            );
            assert_eq!(
                audit.entries[(sequence - 1) as usize].outcome,
                Some(expected)
            );
        }
    }

    #[test]
    fn tool_call_audit_bounds_rows_and_counts_unmatched_calls() {
        use crate::tool_surface::CanonicalToolName as Tool;

        let allowed = vec!["read_file".to_string()];
        let mut audit = ToolCallAudit::with_context(Some("planner"), Some(&allowed));
        for sequence in 1..=62 {
            let call = audit.begin_call(
                sequence,
                "fs/read_text_file",
                Some(Tool::FsReadTextFile),
                sequence - 1,
                0,
            );
            audit.finish_call(call, ToolCallOutcome::Success, None);
        }
        audit.observe_provider_tool_name(&serde_json::json!({
            "sessionUpdate": "tool_call", "title": "orbit_write_file"
        }));
        let denied = audit.begin_call(63, "fs/write_text_file", Some(Tool::FsWriteTextFile), 62, 0);
        audit.finish_call(
            denied,
            ToolCallOutcome::ExpectedDenial,
            Some(crate::tool_surface::ERR_READ_ONLY_ROLE),
        );
        audit.observe_provider_tool_name(&serde_json::json!({
            "sessionUpdate": "tool_call", "title": "orbit_write_file"
        }));
        audit.observe_provider_tool_name(&serde_json::json!({
            "sessionUpdate": "tool_call", "title": "synthetic-secret-title"
        }));
        audit.set_turn_completion(true, 63);

        let entries = &audit.entries;
        assert_eq!(entries.len(), TOOL_CALL_AUDIT_LIMIT);
        assert_eq!(entries[62].advertised_to_provider, Some(false));
        assert_eq!(entries[62].role_allowed, Some(false));
        assert_eq!(entries[62].mutation_applied, Some(false));
        assert_eq!(entries[63].provider_tool_name, "unknown");
        assert_eq!(entries[63].provider_name_mapping, "UNMATCHED");
        assert_eq!(entries[63].advertised_to_provider, None);
        assert_eq!(entries[63].role_allowed, None);
        assert_eq!(entries[63].error_code, Some("PROVIDER_CALLBACK_UNRESOLVED"));
        assert_eq!(audit.omitted_count, 2);
        assert_eq!(audit.mutating_count, 1);
        assert_eq!(audit.mutating_unknown_count, 3);
        assert_eq!(audit.denied_count, 1);

        let metadata = serde_json::json!({
            "tool_call_audit": audit.metadata(63, 62, 1)
        });
        let summary = &metadata["tool_call_audit"]["summary"];
        assert_eq!(summary["total"], 66);
        assert_eq!(summary["successful"], 62);
        assert_eq!(summary["unsuccessful"], 4);
        assert_eq!(summary["mutating"], 1);
        assert_eq!(summary["denied"], 1);
        assert_eq!(summary["unmatched_provider_calls"], 3);
        let encoded = metadata.to_string();
        let report = render_tool_call_audit(&metadata);
        assert!(!encoded.contains("synthetic-secret-title"));
        assert!(!report.contains("synthetic-secret-title"));
        assert!(report.contains("unknown"));
        assert!(report.contains("omitted=2"));
        assert!(report.len() < 128 * 1024);

        let mut overflow = ToolCallAudit::default();
        for _ in 0..=PROVIDER_TOOL_NAME_QUEUE_LIMIT {
            overflow.observe_provider_tool_name(&serde_json::json!({
                "sessionUpdate": "tool_call", "title": "orbit_read_file"
            }));
        }
        overflow.set_turn_completion(true, 0);
        let overflow_report = render_tool_call_audit(&serde_json::json!({
            "tool_call_audit": overflow.metadata(0, 0, 0)
        }));
        assert!(overflow_report.contains("provider titles omitted=1"));
    }

    #[test]
    fn unknown_provider_updates_keep_safe_classification_and_remain_unmatched() {
        use crate::tool_surface::CanonicalToolName as Tool;

        let mut audit = ToolCallAudit::with_context(Some("planner"), None);
        for status in ["in_progress", "completed", "in_progress", "completed"] {
            audit.observe_provider_tool_name(&serde_json::json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "provider-call-123",
                "title": "private-token-shaped-provider-title",
                "kind": "read",
                "status": status,
                "rawInput": {"path": "/private/provider/payload"}
            }));
        }

        for sequence in 1..=2 {
            let call = audit.begin_call(
                sequence,
                "fs/read_text_file",
                Some(Tool::FsReadTextFile),
                sequence - 1,
                0,
            );
            audit.finish_call(call, ToolCallOutcome::Success, None);
        }
        audit.set_turn_completion(true, 2);
        let metadata = serde_json::json!({
            "tool_call_audit": audit.metadata(2, 2, 0)
        });

        assert_eq!(metadata["tool_call_audit"]["summary"]["total"], 6);
        assert_eq!(metadata["tool_call_audit"]["summary"]["successful"], 2);
        assert_eq!(metadata["tool_call_audit"]["summary"]["unsuccessful"], 4);
        assert_eq!(
            metadata["tool_call_audit"]["summary"]["unmatched_provider_calls"],
            4
        );
        assert_eq!(
            metadata["tool_call_audit"]["entries"][0]["provider_update_title_class"],
            "update_not_observed"
        );
        assert_eq!(
            metadata["tool_call_audit"]["entries"][0]["provider_update_tool_kind"],
            "update_not_observed"
        );
        assert_eq!(
            metadata["tool_call_audit"]["entries"][0]["provider_update_status"],
            "update_not_observed"
        );
        assert_eq!(
            metadata["tool_call_audit"]["entries"][0]["provider_tool_call_id_shape"],
            "update_not_observed"
        );
        assert_eq!(
            metadata["tool_call_audit"]["entries"][0]["provider_update_correlation"],
            "CALLBACK_WITHOUT_INVOCATION_ID"
        );
        assert_eq!(
            metadata["tool_call_audit"]["entries"][2]["error_code"],
            "PROVIDER_CALLBACK_UNRESOLVED"
        );
        assert_eq!(
            metadata["tool_call_audit"]["entries"][2]["provider_tool_call_id_shape"],
            "string"
        );
        assert_eq!(
            metadata["tool_call_audit"]["entries"][3]["provider_tool_call_id_shape"],
            "duplicate_string"
        );

        let encoded = metadata.to_string();
        let report = render_tool_call_audit(&metadata);
        assert!(!encoded.contains("private-token-shaped-provider-title"));
        assert!(encoded.contains("provider-call-123"));
        assert!(!encoded.contains("/private/provider/payload"));
        assert!(!report.contains("private-token-shaped-provider-title"));
        assert!(report.contains("provider-call-123"));
        assert!(!report.contains("/private/provider/payload"));
        assert!(report.contains("UNRESOLVED"));
        assert!(report.contains("non_empty_string"));
        assert!(report.contains("in_progress"));
        assert!(report.contains("completed"));
    }

    #[test]
    fn actual_callback_method_maps_independently_of_human_title() {
        use crate::tool_surface::CanonicalToolName as Tool;

        let mut human_title_audit = ToolCallAudit::default();
        human_title_audit.observe_provider_tool_name(&serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "private-id-not-persisted",
            "title": "Reading the project README",
            "kind": "read",
            "status": "in_progress"
        }));
        let call =
            human_title_audit.begin_call(1, "fs/read_text_file", Some(Tool::FsReadTextFile), 0, 0);
        human_title_audit.finish_call(call, ToolCallOutcome::Success, None);
        human_title_audit.set_turn_completion(true, 1);
        assert_eq!(human_title_audit.entries.len(), 2);
        assert_eq!(
            human_title_audit.entries[0].provider_tool_name,
            "fs/read_text_file"
        );
        assert_eq!(human_title_audit.entries[0].provider_name_mapping, "MATCH");
        assert_eq!(
            human_title_audit.entries[0].provider_update_title_class,
            Some("update_not_observed")
        );
        assert_eq!(
            human_title_audit.entries[0].provider_tool_call_id_shape,
            Some("update_not_observed")
        );
        assert_eq!(
            human_title_audit.entries[0].provider_update_correlation,
            "CALLBACK_WITHOUT_INVOCATION_ID"
        );
        assert_eq!(
            human_title_audit.entries[1].provider_tool_call_id_shape,
            Some("string")
        );
        assert_eq!(
            human_title_audit.entries[1].provider_update_correlation,
            "UNMATCHED"
        );
        let encoded = serde_json::to_string(&human_title_audit.entries).unwrap();
        assert!(encoded.contains("private-id-not-persisted"));

        let mut no_update = ToolCallAudit::default();
        let call = no_update.begin_call(1, "fs/read_text_file", Some(Tool::FsReadTextFile), 0, 0);
        no_update.finish_call(call, ToolCallOutcome::Success, None);
        no_update.set_turn_completion(true, 1);
        assert_eq!(no_update.entries[0].provider_tool_name, "fs/read_text_file");
        assert_eq!(no_update.entries[0].provider_name_mapping, "MATCH");
        assert_eq!(
            no_update.entries[0].provider_update_correlation,
            "CALLBACK_WITHOUT_INVOCATION_ID"
        );
        assert_eq!(
            no_update.entries[0].provider_update_status,
            Some("update_not_observed")
        );

        let mut malformed = ToolCallAudit::default();
        let call = malformed.begin_call(1, " fs/read_text_file ", Some(Tool::FsReadTextFile), 0, 0);
        malformed.finish_call(call, ToolCallOutcome::Success, None);
        assert_eq!(malformed.entries[0].provider_tool_name, "unknown");
        assert_eq!(malformed.entries[0].provider_name_mapping, "MISMATCH");

        let mut mismatched = ToolCallAudit::default();
        let call = mismatched.begin_call(1, "fs/write_text_file", Some(Tool::FsReadTextFile), 0, 0);
        mismatched.finish_call(call, ToolCallOutcome::Success, None);
        assert_eq!(
            mismatched.entries[0].provider_tool_name,
            "fs/write_text_file"
        );
        assert_eq!(mismatched.entries[0].provider_name_mapping, "MISMATCH");
    }

    #[test]
    fn duplicate_provider_tool_call_ids_are_detected_and_recorded_safely() {
        use crate::tool_surface::CanonicalToolName as Tool;

        let mut audit = ToolCallAudit::default();
        for sequence in 1..=2 {
            audit.observe_provider_tool_name(&serde_json::json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "private-duplicate-id",
                "title": "Reading a file",
                "kind": "read",
                "status": "in_progress"
            }));
            let call = audit.begin_call(
                sequence,
                "fs/read_text_file",
                Some(Tool::FsReadTextFile),
                sequence - 1,
                0,
            );
            audit.finish_call(call, ToolCallOutcome::Success, None);
        }
        audit.set_turn_completion(true, 2);

        assert_eq!(audit.entries.len(), 4);
        assert_eq!(audit.entries[0].outcome, Some(ToolCallOutcome::Success));
        assert_eq!(audit.entries[1].outcome, Some(ToolCallOutcome::Success));
        assert_eq!(
            audit.entries[0].provider_update_correlation,
            "CALLBACK_WITHOUT_INVOCATION_ID"
        );
        assert_eq!(
            audit.entries[1].provider_update_correlation,
            "CALLBACK_WITHOUT_INVOCATION_ID"
        );
        assert_eq!(audit.entries[2].provider_tool_call_id_shape, Some("string"));
        assert_eq!(
            audit.entries[3].provider_tool_call_id_shape,
            Some("duplicate_string")
        );
        let encoded = serde_json::to_string(&audit.entries).unwrap();
        assert!(encoded.contains("private-duplicate-id"));
    }

    #[test]
    fn equal_count_provider_updates_remain_unmatched_without_shared_callback_ids() {
        use crate::tool_surface::CanonicalToolName as Tool;

        let advertised = vec!["read_file".to_string(), "write_file".to_string()];
        let mut audit = ToolCallAudit::with_context(Some("implementer"), Some(&advertised));
        let read = audit.begin_call(1, "fs/read_text_file", Some(Tool::FsReadTextFile), 0, 0);
        audit.finish_call(read, ToolCallOutcome::Success, None);

        audit.observe_provider_tool_name(&serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "provider-update-one",
            "title": "Edit a file",
            "kind": "edit",
            "status": "in_progress"
        }));
        let write = audit.begin_call(2, "fs/write_text_file", Some(Tool::FsWriteTextFile), 1, 0);
        audit.finish_call(write, ToolCallOutcome::Success, None);

        audit.observe_provider_tool_name(&serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "provider-update-two",
            "title": "Inspect a file",
            "kind": "read",
            "status": "in_progress"
        }));
        audit.set_turn_completion(true, 2);

        let metadata = audit.metadata(2, 2, 0);
        assert_eq!(metadata["summary"]["total"], 4);
        assert_eq!(metadata["summary"]["successful"], 2);
        assert_eq!(metadata["summary"]["unsuccessful"], 2);
        assert_eq!(metadata["summary"]["unmatched_provider_calls"], 2);
        assert_eq!(metadata["entries"][0]["outcome"], "SUCCESS");
        assert_eq!(
            metadata["entries"][0]["provider_tool_name"],
            "fs/read_text_file"
        );
        assert_eq!(
            metadata["entries"][0]["provider_update_correlation"],
            "CALLBACK_WITHOUT_INVOCATION_ID"
        );
        assert_eq!(metadata["entries"][0]["advertised_to_provider"], true);
        assert_eq!(metadata["entries"][0]["role_allowed"], true);
        assert_eq!(metadata["entries"][1]["outcome"], "SUCCESS");
        assert_eq!(
            metadata["entries"][1]["provider_tool_name"],
            "fs/write_text_file"
        );
        assert_eq!(
            metadata["entries"][1]["provider_update_correlation"],
            "CALLBACK_WITHOUT_INVOCATION_ID"
        );
        assert_eq!(
            metadata["entries"][2]["error_code"],
            "PROVIDER_CALLBACK_UNRESOLVED"
        );
        assert_eq!(
            metadata["entries"][3]["error_code"],
            "PROVIDER_CALLBACK_UNRESOLVED"
        );
        let encoded = metadata.to_string();
        assert!(encoded.contains("provider-update-one"));
        assert!(encoded.contains("provider-update-two"));
        assert!(!encoded.contains("Edit a file"));
        assert!(!encoded.contains("Inspect a file"));
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
    fn handoff_requires_successful_supervisor_and_matching_cleanup() {
        use std::os::unix::process::ExitStatusExt;
        let handoff = format!("{ORBIT_HANDOFF_START}\n{{}}\n{ORBIT_HANDOFF_END}");
        let nonzero = classify_supervisor_evidence(
            std::process::ExitStatus::from_raw(7 << 8),
            Ok(cleanup_receipt(7)),
        );
        assert!(nonzero.cleanup_confirmed);
        assert_eq!(nonzero.exit_code, Some(7));
        assert_eq!(nonzero.signal, None);
        assert!(
            validate_role_turn_completion(&handoff, &nonzero)
                .unwrap_err()
                .to_string()
                .contains("SUPERVISOR_NONZERO_EXIT")
        );

        let signal = classify_supervisor_evidence(
            std::process::ExitStatus::from_raw(libc::SIGTERM),
            Ok(cleanup_receipt(0)),
        );
        assert_eq!(signal.exit_code, None);
        assert_eq!(signal.signal, Some(libc::SIGTERM));
        assert!(signal.cleanup_confirmed);
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
                .contains("CLEANUP_RECEIPT_UNCONFIRMED")
        );

        let successful = classify_supervisor_evidence(
            std::process::ExitStatus::from_raw(0),
            Ok(cleanup_receipt(0)),
        );
        assert!(validate_role_turn_completion(&handoff, &successful).is_ok());
    }

    #[tokio::test]
    async fn lifecycle_persistence_failure_still_kills_and_reaps_supervisor() -> Result<()> {
        let scratch = tempdir()?;
        let request = scratch.path().join("request.json");
        std::fs::write(&request, b"{}").unwrap();
        let mut command = tokio::process::Command::new("sh");
        command
            .args(["-c", "sleep 30"])
            .process_group(0)
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let pid = child.id().context("child PID missing")?;
        let started = tokio::time::Instant::now();
        let evidence = terminate_supervisor_after_evidence_failure(
            &mut child,
            &request,
            "attempt-test",
            "pinned-image",
        )
        .await;
        assert!(started.elapsed() < Duration::from_secs(7));
        assert!(evidence.exit_code.is_none());
        assert_eq!(evidence.signal, Some(libc::SIGKILL));
        assert!(!evidence.cleanup_confirmed);
        assert!(evidence.receipt.is_none());
        assert!(
            child.try_wait()?.is_some(),
            "supervisor child {pid} was not reaped"
        );
        Ok(())
    }

    #[tokio::test]
    async fn supervisor_wait_is_bounded_and_kills_process_group() -> Result<()> {
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
    fn wrong_credential_generation_is_rejected() {
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
