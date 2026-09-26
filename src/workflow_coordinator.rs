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
        EnvironmentIdentity, VerificationPlan, VerificationPolicy, VerificationRun,
        VerificationRunResult, VerificationStep, VerificationStore, WorkspaceState,
        execute_run_contents,
    },
    workflow::*,
};
use anyhow::{Context, Result, bail, ensure};
use sqlx::PgPool;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::process::Stdio;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

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
    ) -> Result<RoleExecutionOutcome>;
}

/// Production Workflow Coordinator driving workflow runs to completion.
pub struct WorkflowCoordinator {
    pool: PgPool,
    store: WorkflowStore,
    verification_store: VerificationStore,
    regression_store: RegressionStore,
    executor: Arc<dyn RoleAgentExecutor>,
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
        }
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
        let wf = self
            .store
            .get_workflow_run(wf_id)
            .await?
            .context("workflow run not found")?;

        if wf.status.is_terminal() {
            return Ok(WorkflowStepResult::Terminal(wf.status));
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
                let target =
                    RoleRuntimeResolver::resolve_target_live(&self.pool, &role, None).await?;

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
                        Path::new(wf.repository_path.as_deref().unwrap_or(".")),
                        None,
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
                let lock_holder_id = format!("{}-impl-{}", wf.id, wf.iteration);
                self.store
                    .acquire_workspace_mutation_lock(&wf.attempt_id, &lock_holder_id)
                    .await?;

                let role = RoleDefinition::implementer_v1();
                let target =
                    RoleRuntimeResolver::resolve_target_live(&self.pool, &role, None).await?;

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

                let outcome = self
                    .executor
                    .execute_role(
                        &self.pool,
                        &wf,
                        &role_exec,
                        &role,
                        &target,
                        wf.task_prompt.as_deref().unwrap_or(""),
                        Path::new(wf.repository_path.as_deref().unwrap_or(".")),
                        plan_handoff.as_ref(),
                    )
                    .await;

                let outcome = match outcome {
                    Ok(o) => o,
                    Err(e) => {
                        let _ = self
                            .store
                            .release_workspace_mutation_lock(&wf.attempt_id, &lock_holder_id)
                            .await;
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

                let repo_path = Path::new(wf.repository_path.as_deref().unwrap_or("."));
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

                let repo_path = Path::new(wf.repository_path.as_deref().unwrap_or("."));
                let baseline = wf.base_revision.as_deref().unwrap_or("HEAD");
                let ws_state = compute_workspace_state(repo_path, baseline).await?;

                let policy = if let (Some(id), Some(ver)) =
                    (&wf.verification_policy_id, wf.verification_policy_version)
                {
                    self.verification_store.get_policy(id, ver).await?
                } else {
                    None
                };

                let reg_policy = if let (Some(id), Some(ver)) =
                    (&wf.regression_policy_id, wf.regression_policy_version)
                {
                    self.regression_store.get_regression_policy(id, ver).await?
                } else {
                    None
                };

                let sel_policy = if let (Some(id), Some(ver)) =
                    (&wf.selection_policy_id, wf.selection_policy_version)
                {
                    self.regression_store.get_selection_policy(id, ver).await?
                } else {
                    None
                };

                // Run FAST tier verification first
                let fast_run = self
                    .run_tier_verification(
                        &wf,
                        &ws_state,
                        VerificationTier::Fast,
                        policy.as_ref(),
                        reg_policy.as_ref(),
                        sel_policy.as_ref(),
                    )
                    .await?;

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
                        policy.as_ref(),
                        reg_policy.as_ref(),
                        sel_policy.as_ref(),
                    )
                    .await?;

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
                sqlx::query("UPDATE orbit_workflow_runs SET iteration = $1 WHERE id = $2")
                    .bind(next_iteration as i32)
                    .bind(wf_id)
                    .execute(&self.pool)
                    .await?;

                let lock_holder_id = format!("{}-repair-{}", wf.id, next_iteration);
                self.store
                    .acquire_workspace_mutation_lock(&wf.attempt_id, &lock_holder_id)
                    .await?;

                let role = RoleDefinition::implementer_v1();
                let target =
                    RoleRuntimeResolver::resolve_target_live(&self.pool, &role, None).await?;

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

                let outcome = self
                    .executor
                    .execute_role(
                        &self.pool,
                        &wf,
                        &role_exec,
                        &role,
                        &target,
                        wf.task_prompt.as_deref().unwrap_or(""),
                        Path::new(wf.repository_path.as_deref().unwrap_or(".")),
                        failure_handoff.as_ref(),
                    )
                    .await;

                let outcome = match outcome {
                    Ok(o) => o,
                    Err(e) => {
                        let _ = self
                            .store
                            .release_workspace_mutation_lock(&wf.attempt_id, &lock_holder_id)
                            .await;
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

                let repo_path = Path::new(wf.repository_path.as_deref().unwrap_or("."));
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
                let role = RoleDefinition::reviewer_v1();
                let target =
                    RoleRuntimeResolver::resolve_target_live(&self.pool, &role, None).await?;

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
                        Path::new(wf.repository_path.as_deref().unwrap_or(".")),
                        impl_handoff.as_ref(),
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

                let repo_path = Path::new(wf.repository_path.as_deref().unwrap_or("."));
                let baseline = wf.base_revision.as_deref().unwrap_or("HEAD");
                let ws_state = compute_workspace_state(repo_path, baseline).await?;

                let policy = if let (Some(id), Some(ver)) =
                    (&wf.verification_policy_id, wf.verification_policy_version)
                {
                    self.verification_store.get_policy(id, ver).await?
                } else {
                    None
                };

                let reg_policy = if let (Some(id), Some(ver)) =
                    (&wf.regression_policy_id, wf.regression_policy_version)
                {
                    self.regression_store.get_regression_policy(id, ver).await?
                } else {
                    None
                };

                let sel_policy = if let (Some(id), Some(ver)) =
                    (&wf.selection_policy_id, wf.selection_policy_version)
                {
                    self.regression_store.get_selection_policy(id, ver).await?
                } else {
                    None
                };

                // Run FULL tier regression verification
                let full_run = self
                    .run_tier_verification(
                        &wf,
                        &ws_state,
                        VerificationTier::Full,
                        policy.as_ref(),
                        reg_policy.as_ref(),
                        sel_policy.as_ref(),
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

    /// Cancel a workflow run, release any attempt workspace locks, and mark role executions cancelled.
    pub async fn cancel_workflow(&self, wf_id: &str, reason: &str) -> Result<WorkflowRun> {
        let wf = self
            .store
            .get_workflow_run(wf_id)
            .await?
            .context("workflow run not found")?;

        // Release any workspace mutation locks held for this attempt
        let _ = sqlx::query("DELETE FROM orbit_attempt_workspace_locks WHERE attempt_id = $1")
            .bind(&wf.attempt_id)
            .execute(&self.pool)
            .await;

        let now_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as i64;

        // Cancel running role executions
        let _ = sqlx::query(
            "UPDATE orbit_role_executions SET status = 'CANCELLED', termination_reason = $1, finished_at_ms = $2 WHERE workflow_run_id = $3 AND status IN ('PENDING', 'RUNNING')",
        )
        .bind(reason)
        .bind(now_ms)
        .bind(wf_id)
        .execute(&self.pool)
        .await;

        self.store
            .transition_workflow_stage(
                wf_id,
                WorkflowStage::Cancelled,
                wf.current_workspace_state_id.as_deref(),
                None,
                Some(reason),
            )
            .await?;

        let updated = self
            .store
            .get_workflow_run(wf_id)
            .await?
            .context("workflow run not found after cancel")?;
        Ok(updated)
    }

    /// Helper to record failure evidence as a handoff artifact for repair stages.
    async fn record_failure_evidence(
        &self,
        wf_id: &str,
        stage: &str,
        verif_run_id: Option<&str>,
        error_msg: &str,
    ) -> Result<()> {
        let evidence = FailureEvidenceHandoff {
            failed_stage: stage.to_string(),
            verification_run_id: verif_run_id.map(|s| s.to_string()),
            failed_steps: vec![stage.to_string()],
            error_summary: error_msg.to_string(),
            stdout_previews: BTreeMap::new(),
            stderr_previews: BTreeMap::new(),
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
        let repo_path = Path::new(wf.repository_path.as_deref().unwrap_or("."));
        let env = EnvironmentIdentity::default();

        if let Some(sp) = sel_policy {
            let selected_plan =
                select_verification(sp, reg_policy, &ws_state.state_id, tier, &[], &[], &[])?;
            crate::regression_strategy::execute_selected_verification_plan(
                &self.verification_store,
                &wf.attempt_id,
                ws_state,
                &selected_plan,
                repo_path,
                env,
                reg_policy,
                None,
            )
            .await
        } else if let Some(p) = policy {
            let steps = if !p.required_steps.is_empty() {
                p.required_steps
                    .iter()
                    .map(|s| {
                        VerificationStep::new_command(
                            s,
                            s,
                            vec!["git".into(), "diff".into(), "--check".into()],
                        )
                    })
                    .collect()
            } else {
                vec![VerificationStep::new_command(
                    "git-diff-check",
                    "verify workspace diff formatting and cleanliness",
                    vec!["git".into(), "diff".into(), "--check".into()],
                )]
            };
            let plan = VerificationPlan::new(
                "tier-policy-plan",
                format!("{:?} Tier Verification", tier),
                steps,
            );
            let run = self
                .verification_store
                .create_run_with_policy_and_tier(
                    &wf.attempt_id,
                    ws_state,
                    &plan,
                    env,
                    Some(p),
                    Some(tier),
                    None,
                    reg_policy,
                )
                .await?;
            execute_run_contents(
                &self.verification_store,
                run,
                ws_state,
                &plan,
                repo_path,
                Some(p),
                None,
            )
            .await
        } else if repo_path
            .join(".orbit/definitions/docs-workflow.yaml")
            .exists()
        {
            let plan = VerificationPlan::new(
                "tier-docs-plan",
                format!("{:?} Tier Documentation Verification", tier),
                vec![VerificationStep::new_command(
                    "docs-presence-check",
                    "verify documentation directories exist",
                    vec!["sh".into(), "-c".into(), "test -d docs".into()],
                )],
            );
            let authoritative_policy = VerificationPolicy::new(
                "docs-workflow-authoritative",
                "Authoritative Documentation Verification Policy",
            );
            let run = self
                .verification_store
                .create_run_with_policy_and_tier(
                    &wf.attempt_id,
                    ws_state,
                    &plan,
                    env,
                    Some(&authoritative_policy),
                    Some(tier),
                    None,
                    reg_policy,
                )
                .await?;
            execute_run_contents(
                &self.verification_store,
                run,
                ws_state,
                &plan,
                repo_path,
                Some(&authoritative_policy),
                None,
            )
            .await
        } else if repo_path.join("Cargo.toml").exists() {
            let plan = VerificationPlan::new(
                "tier-cargo-plan",
                format!("{:?} Tier Cargo Verification", tier),
                vec![VerificationStep::new_command(
                    "git-diff-check",
                    "verify workspace git diff formatting",
                    vec!["git".into(), "diff".into(), "--check".into()],
                )],
            );
            let authoritative_policy = VerificationPolicy::new(
                "cargo-workspace-authoritative",
                "Authoritative Cargo Workspace Verification Policy",
            );
            let run = self
                .verification_store
                .create_run_with_policy_and_tier(
                    &wf.attempt_id,
                    ws_state,
                    &plan,
                    env,
                    Some(&authoritative_policy),
                    Some(tier),
                    None,
                    reg_policy,
                )
                .await?;
            execute_run_contents(
                &self.verification_store,
                run,
                ws_state,
                &plan,
                repo_path,
                Some(&authoritative_policy),
                None,
            )
            .await
        } else {
            bail!(
                "VERIFICATION_POLICY_REQUIRED: no authoritative verification policy provided or resolved for workflow run"
            );
        }
    }
}

/// Computes or captures current WorkspaceState for a given repository.
pub async fn compute_workspace_state(
    repo_path: &Path,
    baseline_revision: &str,
) -> Result<WorkspaceState> {
    if repo_path.exists() && repo_path.join(".git").exists() {
        let head_out = tokio::process::Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(["rev-parse", "HEAD"])
            .output()
            .await?;
        let head_revision = String::from_utf8(head_out.stdout)
            .unwrap_or_else(|_| "HEAD".into())
            .trim()
            .to_string();

        let diff_out = tokio::process::Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args([
                "diff",
                "--binary",
                "--no-ext-diff",
                "--full-index",
                baseline_revision,
                "--",
            ])
            .output()
            .await?;

        let diff_sha256 = if diff_out.stdout.is_empty() {
            None
        } else {
            Some(crate::model::digest(&diff_out.stdout))
        };

        Ok(WorkspaceState::compute_from_parts(
            baseline_revision,
            &head_revision,
            diff_sha256.as_deref(),
        ))
    } else {
        // Fallback for tests or synthetic directories:
        // Hash file contents deterministically
        let mut entries = Vec::new();
        if let Ok(rd) = std::fs::read_dir(repo_path) {
            for entry in rd.flatten() {
                let path = entry.path();
                if path.is_file() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let bytes = std::fs::read(&path).unwrap_or_default();
                    entries.push(format!("{}:{}", name, digest(&bytes)));
                }
            }
        }
        entries.sort();
        let diff_hash = if entries.is_empty() {
            None
        } else {
            Some(digest(entries.join(";").as_bytes()))
        };
        Ok(WorkspaceState::compute_from_parts(
            baseline_revision,
            "HEAD",
            diff_hash.as_deref(),
        ))
    }
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

    match role.role_id.as_str() {
        "planner" => format!(
            "You are the PLANNER role in an Orbit automated software change workflow.\n            Your responsibility is to analyze the task, inspect the repository using read_file, and produce a clear, structured implementation plan.\n\n            TASK OBJECTIVE:\n{task_text}\n\n            REPOSITORY CONTEXT:\n            Repository Path: {repo_path}\n            Base Revision: {base_revision}{docs_manifest}\n            WORKSPACE PERMISSIONS:\n            You have READ-ONLY workspace access. You can inspect files using read_file.\n            You CANNOT write files. Any write requests will be rejected by the workspace broker.\n\n            INSTRUCTIONS:\n            1. Use read_file with workspace-relative file paths (e.g. docs/README.md) to inspect files.\n               Important: do NOT pass directory paths or root paths to read_file; read_file only reads text files.\n            2. Formulate a concrete step-by-step implementation plan.\n            3. You MUST end your response with a structured JSON plan handoff block inside the exact delimiters:\n            <<<ORBIT_HANDOFF_START>>>\n            {{\n              \"summary\": \"Concise summary of the plan\",\n              \"affected_areas\": [\"area1\", \"area2\"],\n              \"implementation_steps\": [\"step 1\", \"step 2\"],\n              \"expected_files\": [\"docs/file1.md\"],\n              \"risks\": [],\n              \"verification_notes\": [\"verification instructions\"],\n              \"open_questions\": []\n            }}\n            <<<ORBIT_HANDOFF_END>>>\n",
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
                "You are the IMPLEMENTER role in an Orbit automated software change workflow.\n                Your responsibility is to execute the implementation plan by modifying project files and verifying your work.\n\n                TASK OBJECTIVE:\n{task_text}\n\n                PLANNER SPECIFICATION:\n{plan_summary}\n\n                REPOSITORY CONTEXT:\n                Repository Path: {repo_path}\n                Base Revision: {base_revision}{docs_manifest}\n                WORKSPACE PERMISSIONS:\n                You have READ-WRITE workspace access. You can read files using read_file, create/edit files using write_file, create directories using create_directory, move or rename files and directories using move, delete files using delete_file, and remove directories using delete_directory.\n\n                INSTRUCTIONS:\n                1. Workspace operations:\n                   - read_file: inspect file contents (workspace-relative path, e.g. docs/README.md).\n                   - write_file: create or update file contents (keep each under 64 KiB).\n                   - create_directory: create a directory (e.g. docs/archive).\n                   - move: move or rename a file or directory (e.g. source: docs/old.md, destination: docs/archive/old.md).\n                   - delete_file: delete a single file.\n                   - delete_directory: remove an empty directory (or recursive if specified).\n                   Important: Always use workspace-relative paths. Do NOT pass directory paths to read_file or write_file.\n                2. Implement all required changes and directory reorganization per the planner specification.\n                3. You MUST end your response with a structured JSON implementation handoff block inside the exact delimiters:\n                <<<ORBIT_HANDOFF_START>>>\n                {{\n                  \"summary\": \"Concise summary of changes implemented\",\n                  \"changed_files\": [\"docs/file1.md\"],\n                  \"tests_added_or_modified\": [],\n                  \"exploratory_commands\": [],\n                  \"known_limitations\": [],\n                  \"verification_notes\": [\"self-verification details\"]\n                }}\n                <<<ORBIT_HANDOFF_END>>>\n",
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
    }
}

struct AcpTurnState<'a> {
    repo_path: &'a Path,
    workspace_access: WorkspaceAccess,
    agent_output: String,
    tool_calls: u64,
    tool_successes: u64,
    tool_failures: u64,
    tool_counts: BTreeMap<String, u64>,
}

fn resolve_workspace_path(repo_path: &Path, requested: &str) -> PathBuf {
    let clean = requested.trim();
    if let Some(stripped) = clean.strip_prefix("/orbit/home/workspace/") {
        repo_path.join(stripped)
    } else if clean == "/orbit/home/workspace" {
        repo_path.to_path_buf()
    } else if let Some(stripped) = clean.strip_prefix("/orbit/home/") {
        repo_path.join(stripped)
    } else if clean == "/orbit/home" {
        repo_path.to_path_buf()
    } else {
        let p = Path::new(clean);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            repo_path.join(p)
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

async fn handle_acp_message(
    wire: &mut Wire,
    state: &mut AcpTurnState<'_>,
    message: serde_json::Value,
) -> Result<()> {
    if let Some(method) = message.get("method").and_then(|m| m.as_str()) {
        match method {
            "session/update" => {
                if let Some(params) = message.get("params") {
                    if let Some(update) = params.get("update") {
                        extract_text_from_json(update, &mut state.agent_output);
                    } else {
                        extract_text_from_json(params, &mut state.agent_output);
                    }
                }
                Ok(())
            }
            "fs/read_text_file" => {
                let req_id = message
                    .get("id")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                state.tool_calls += 1;
                *state.tool_counts.entry("read_file".into()).or_insert(0) += 1;

                let rel_path_str = message
                    .get("params")
                    .and_then(|p| p.get("path"))
                    .and_then(|p| p.as_str())
                    .unwrap_or("");
                let full_path = resolve_workspace_path(state.repo_path, rel_path_str);

                let canonical_repo = match state.repo_path.canonicalize() {
                    Ok(c) => c,
                    Err(_) => state.repo_path.to_path_buf(),
                };
                let canonical_full = match full_path.canonicalize() {
                    Ok(c) => c,
                    Err(_) => full_path.clone(),
                };

                if !canonical_full.starts_with(&canonical_repo) {
                    state.tool_failures += 1;
                    wire.response_error(req_id, -32603, "access denied: path outside workspace")
                        .await?;
                    return Ok(());
                }

                match tokio::fs::read_to_string(&full_path).await {
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
                        wire.response_ok(req_id, serde_json::json!({ "content": bounded_content }))
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
                Ok(())
            }
            "fs/write_text_file" => {
                let req_id = message
                    .get("id")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                state.tool_calls += 1;
                *state.tool_counts.entry("write_file".into()).or_insert(0) += 1;

                if state.workspace_access == WorkspaceAccess::ReadOnly {
                    state.tool_failures += 1;
                    wire.response_error(
                        req_id,
                        -32603,
                        "write operation denied: read-only role workspace",
                    )
                    .await?;
                    return Ok(());
                }

                let rel_path_str = message
                    .get("params")
                    .and_then(|p| p.get("path"))
                    .and_then(|p| p.as_str())
                    .unwrap_or("");
                let content = message
                    .get("params")
                    .and_then(|p| p.get("content"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("");

                let full_path = resolve_workspace_path(state.repo_path, rel_path_str);

                let canonical_repo = match state.repo_path.canonicalize() {
                    Ok(c) => c,
                    Err(_) => state.repo_path.to_path_buf(),
                };
                let is_safe = match full_path.parent().map(|p| p.canonicalize()) {
                    Some(Ok(c)) => c.starts_with(&canonical_repo),
                    _ => {
                        let mut cur = full_path.parent();
                        let mut safe = true;
                        while let Some(p) = cur {
                            if let Ok(c) = p.canonicalize() {
                                safe = c.starts_with(&canonical_repo);
                                break;
                            }
                            cur = p.parent();
                        }
                        safe
                    }
                };

                if !is_safe {
                    state.tool_failures += 1;
                    wire.response_error(req_id, -32603, "access denied: path outside workspace")
                        .await?;
                    return Ok(());
                }

                if let Some(parent) = full_path.parent() {
                    let _ = tokio::fs::create_dir_all(parent).await;
                }

                match tokio::fs::write(&full_path, content).await {
                    Ok(()) => {
                        state.tool_successes += 1;
                        wire.response_ok(req_id, serde_json::json!({})).await?;
                    }
                    Err(e) => {
                        state.tool_failures += 1;
                        wire.response_error(req_id, -32603, &format!("failed to write file: {e}"))
                            .await?;
                    }
                }
                Ok(())
            }
            "fs/create_directory" => {
                let req_id = message
                    .get("id")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                state.tool_calls += 1;
                *state
                    .tool_counts
                    .entry("create_directory".into())
                    .or_insert(0) += 1;

                if state.workspace_access == WorkspaceAccess::ReadOnly {
                    state.tool_failures += 1;
                    wire.response_error(
                        req_id,
                        -32603,
                        "write operation denied: read-only role workspace",
                    )
                    .await?;
                    return Ok(());
                }

                let path_str = message
                    .get("params")
                    .and_then(|p| p.get("path"))
                    .and_then(|p| p.as_str())
                    .unwrap_or("");
                let recursive = message
                    .get("params")
                    .and_then(|p| p.get("recursive"))
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
                Ok(())
            }
            "fs/move" => {
                let req_id = message
                    .get("id")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                state.tool_calls += 1;
                *state.tool_counts.entry("move".into()).or_insert(0) += 1;

                if state.workspace_access == WorkspaceAccess::ReadOnly {
                    state.tool_failures += 1;
                    wire.response_error(
                        req_id,
                        -32603,
                        "write operation denied: read-only role workspace",
                    )
                    .await?;
                    return Ok(());
                }

                let source_str = message
                    .get("params")
                    .and_then(|p| p.get("source"))
                    .and_then(|p| p.as_str())
                    .unwrap_or("");
                let destination_str = message
                    .get("params")
                    .and_then(|p| p.get("destination"))
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
                Ok(())
            }
            "fs/delete_file" => {
                let req_id = message
                    .get("id")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                state.tool_calls += 1;
                *state.tool_counts.entry("delete_file".into()).or_insert(0) += 1;

                if state.workspace_access == WorkspaceAccess::ReadOnly {
                    state.tool_failures += 1;
                    wire.response_error(
                        req_id,
                        -32603,
                        "write operation denied: read-only role workspace",
                    )
                    .await?;
                    return Ok(());
                }

                let path_str = message
                    .get("params")
                    .and_then(|p| p.get("path"))
                    .and_then(|p| p.as_str())
                    .unwrap_or("");

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
                Ok(())
            }
            "fs/delete_directory" => {
                let req_id = message
                    .get("id")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                state.tool_calls += 1;
                *state
                    .tool_counts
                    .entry("delete_directory".into())
                    .or_insert(0) += 1;

                if state.workspace_access == WorkspaceAccess::ReadOnly {
                    state.tool_failures += 1;
                    wire.response_error(
                        req_id,
                        -32603,
                        "write operation denied: read-only role workspace",
                    )
                    .await?;
                    return Ok(());
                }

                let path_str = message
                    .get("params")
                    .and_then(|p| p.get("path"))
                    .and_then(|p| p.as_str())
                    .unwrap_or("");
                let recursive = message
                    .get("params")
                    .and_then(|p| p.get("recursive"))
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
                Ok(())
            }
            other => {
                if let Some(id) = message.get("id").cloned() {
                    wire.response_error(id, -32601, &format!("unsupported ACP method: {other}"))
                        .await?;
                }
                Ok(())
            }
        }
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
) -> Result<RoleExecutionOutcome> {
    let agent_exec_id = format!("acp-exec-{}", id());
    let started_at_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as i64;

    let cred_store = CredentialStore::new(pool);
    let credential = if let Some(cid) = &target.credential_id {
        cred_store
            .get(cid)
            .await?
            .context("target credential not found")?
    } else {
        let cred_view = cred_store
            .list()
            .await?
            .into_iter()
            .find(|c| {
                c.provider == target.provider
                    && c.status == crate::credential_registry::CredentialStatus::Enrolled
            })
            .context("no enrolled credential found for provider")?;
        cred_store
            .get(&cred_view.reference)
            .await?
            .context("credential not found")?
    };

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

    if let Some(val) = std::env::var_os("XDG_RUNTIME_DIR") {
        command.env("XDG_RUNTIME_DIR", val);
    }

    let mut child = command.spawn().context("failed to spawn acp-supervisor")?;

    let child_out = child.stdout.take().context("child stdout missing")?;
    let child_in = child.stdin.take().context("child stdin missing")?;
    let mut wire = Wire::new(child_out, child_in, 16 * 1024 * 1024);

    let mut state = AcpTurnState {
        repo_path,
        workspace_access: role.workspace_access,
        agent_output: String::new(),
        tool_calls: 0,
        tool_successes: 0,
        tool_failures: 0,
        tool_counts: BTreeMap::new(),
    };

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
                    "writeTextFile": role.workspace_access == WorkspaceAccess::ReadWrite
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
        let base = wf_run.base_revision.as_deref().unwrap_or("HEAD");
        let out = tokio::process::Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(["diff", "--no-ext-diff", base, "--"])
            .output()
            .await;
        match out {
            Ok(o) => String::from_utf8(o.stdout).ok(),
            Err(_) => None,
        }
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

    drop(wire);
    let status = child.wait().await?;
    let exit_code = status.code().unwrap_or(0);
    let finished_at_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as i64;

    let store = WorkflowStore::new(pool.clone());

    if !state.agent_output.contains(ORBIT_HANDOFF_START)
        || !state.agent_output.contains(ORBIT_HANDOFF_END)
    {
        let _ = store
            .insert_agent_execution(
                &agent_exec_id,
                &role_exec.id,
                &format!("{}-acp", target.provider),
                Some(&target.provider),
                target.resolved_model.as_deref(),
                started_at_ms,
                Some(finished_at_ms),
                "FAILED",
                Some("ROLE_OUTPUT_INVALID"),
                Some(exit_code),
                Some("Missing structured handoff block in agent output"),
                target.resolved_model.as_deref(),
                target.resolved_model.as_deref(),
                target.resolved_model.as_deref(),
                1,
                state.tool_calls as i64,
                state.tool_successes as i64,
                state.tool_failures as i64,
                &serde_json::to_value(&state.tool_counts)?,
                &serde_json::json!({ "provider": target.provider }),
            )
            .await;
        let _ = store
            .record_agent_execution(&role_exec.id, &agent_exec_id)
            .await;
        bail!(
            "ROLE_OUTPUT_INVALID: missing structured handoff block <<<ORBIT_HANDOFF_START>>> in ACP agent output"
        );
    }

    store
        .insert_agent_execution(
            &agent_exec_id,
            &role_exec.id,
            &format!("{}-acp", target.provider),
            Some(&target.provider),
            target.resolved_model.as_deref(),
            started_at_ms,
            Some(finished_at_ms),
            "SUCCEEDED",
            Some("completed"),
            Some(exit_code),
            None,
            target.resolved_model.as_deref(),
            target.resolved_model.as_deref(),
            target.resolved_model.as_deref(),
            1,
            state.tool_calls as i64,
            state.tool_successes as i64,
            state.tool_failures as i64,
            &serde_json::to_value(&state.tool_counts)?,
            &serde_json::json!({ "provider": target.provider }),
        )
        .await?;
    store
        .record_agent_execution(&role_exec.id, &agent_exec_id)
        .await?;

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

        // Check if mock mode is requested via env or if we run directly
        if std::env::var("ORBIT_MOCK_ACP").as_deref() == Ok("1") {
            let sim = SimulatedRoleExecutor::with_approval();
            return sim
                .execute_role(
                    pool,
                    wf_run,
                    role_exec,
                    role,
                    target,
                    task_text,
                    repo_path,
                    input_handoff,
                )
                .await;
        }

        execute_real_acp_turn(
            pool,
            wf_run,
            role_exec,
            role,
            target,
            task_text,
            repo_path,
            input_handoff,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn make_test_wire() -> (Wire, Wire) {
        let (client_r, server_w) = tokio::io::duplex(65536);
        let (server_r, client_w) = tokio::io::duplex(65536);
        let server_wire = Wire::new(server_r, server_w, 65536);
        let client_wire = Wire::new(client_r, client_w, 65536);
        (server_wire, client_wire)
    }

    #[tokio::test]
    async fn test_handle_acp_message_filesystem_mutations() -> Result<()> {
        let repo = tempdir()?;
        let repo_path = repo.path();
        let (mut server_wire, mut client_wire) = make_test_wire();

        let mut state = AcpTurnState {
            repo_path,
            workspace_access: WorkspaceAccess::ReadWrite,
            agent_output: String::new(),
            tool_calls: 0,
            tool_successes: 0,
            tool_failures: 0,
            tool_counts: BTreeMap::new(),
        };

        // 1. Create directory
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
        assert_eq!(resp["result"]["success"], true);
        assert!(repo_path.join("docs/archive").is_dir());

        // 2. Create file to move
        fs::write(repo_path.join("docs/old.md"), "content")?;

        // 3. Move file
        let msg = serde_json::json!({
            "id": 2,
            "method": "fs/move",
            "params": {
                "source": "docs/old.md",
                "destination": "docs/archive/old.md"
            }
        });
        handle_acp_message(&mut server_wire, &mut state, msg).await?;
        let resp = client_wire.read().await?;
        assert_eq!(resp["id"], 2);
        assert_eq!(resp["result"]["success"], true);
        assert!(repo_path.join("docs/archive/old.md").is_file());
        assert!(!repo_path.join("docs/old.md").exists());

        // 4. Delete file
        let msg = serde_json::json!({
            "id": 3,
            "method": "fs/delete_file",
            "params": {
                "path": "docs/archive/old.md"
            }
        });
        handle_acp_message(&mut server_wire, &mut state, msg).await?;
        let resp = client_wire.read().await?;
        assert_eq!(resp["id"], 3);
        assert_eq!(resp["result"]["success"], true);
        assert!(!repo_path.join("docs/archive/old.md").exists());

        // 5. Delete directory
        let msg = serde_json::json!({
            "id": 4,
            "method": "fs/delete_directory",
            "params": {
                "path": "docs/archive",
                "recursive": false
            }
        });
        handle_acp_message(&mut server_wire, &mut state, msg).await?;
        let resp = client_wire.read().await?;
        assert_eq!(resp["id"], 4);
        assert_eq!(resp["result"]["success"], true);
        assert!(!repo_path.join("docs/archive").exists());

        assert_eq!(state.tool_calls, 4);
        assert_eq!(state.tool_successes, 4);
        assert_eq!(state.tool_failures, 0);
        assert_eq!(state.tool_counts["create_directory"], 1);
        assert_eq!(state.tool_counts["move"], 1);
        assert_eq!(state.tool_counts["delete_file"], 1);
        assert_eq!(state.tool_counts["delete_directory"], 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_handle_acp_message_read_only_denial() -> Result<()> {
        let repo = tempdir()?;
        let repo_path = repo.path();
        let (mut server_wire, mut client_wire) = make_test_wire();

        let mut state = AcpTurnState {
            repo_path,
            workspace_access: WorkspaceAccess::ReadOnly,
            agent_output: String::new(),
            tool_calls: 0,
            tool_successes: 0,
            tool_failures: 0,
            tool_counts: BTreeMap::new(),
        };

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
}
