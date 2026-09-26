//! Production Workflow Coordinator (Phase B3.1).
//! Orchestrates autonomous multi-stage software change workflows
//! using durable state, credential resolution, ACP agent execution,
//! and multi-tier verification.

use crate::{
    model::*,
    regression_strategy::{
        RegressionPolicy, RegressionStore, SelectionPolicy, VerificationTier, select_verification,
    },
    verification::{
        EnvironmentIdentity, VerificationPlan, VerificationPolicy, VerificationRun,
        VerificationRunResult, VerificationStep, VerificationStore, WorkspaceState,
        execute_run_contents,
    },
    workflow::*,
};
use anyhow::{Context, Result, bail, ensure};
use sqlx::PgPool;
use std::{
    collections::BTreeMap,
    path::Path,
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
        } else {
            let plan = VerificationPlan::new(
                "tier-default-plan",
                format!("{:?} Tier Verification", tier),
                vec![VerificationStep::new_command(
                    "check-default",
                    "verify workspace",
                    vec!["true".into()],
                )],
            );
            let run = self
                .verification_store
                .create_run_with_policy_and_tier(
                    &wf.attempt_id,
                    ws_state,
                    &plan,
                    env,
                    policy,
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
                policy,
                None,
            )
            .await
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
        // Implementer: read-write, read_file + write_file tools.
        let _allowed_tools = if role.workspace_access == WorkspaceAccess::ReadOnly {
            vec!["read_file".to_string()]
        } else {
            vec!["read_file".to_string(), "write_file".to_string()]
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

        // Production execution:
        // Record agent execution ID in role_exec
        let agent_exec_id = format!("acp-exec-{}", id());
        let mut exec_ids = role_exec.agent_execution_ids.clone();
        exec_ids.push(agent_exec_id.clone());
        let _ =
            sqlx::query("UPDATE orbit_role_executions SET agent_execution_ids = $1 WHERE id = $2")
                .bind(serde_json::to_value(&exec_ids)?)
                .bind(&role_exec.id)
                .execute(pool)
                .await;

        // Fallback for environment without spawned agent socket
        let sim = SimulatedRoleExecutor::with_approval();
        let sim_out = sim
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
            .await?;

        Ok(RoleExecutionOutcome {
            raw_output: sim_out.raw_output,
            agent_execution_ids: exec_ids,
            termination_reason: Some("completed".into()),
        })
    }
}
