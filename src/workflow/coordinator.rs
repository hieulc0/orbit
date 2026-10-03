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
    role_prompt::{RolePromptToolContext, build_role_prompt, repository_tools_for_role},
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

/// Whether a role is required to produce a new repository candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImplementationMutationRequirement {
    Required,
    NoChangeAllowed,
}

/// Authoritative result of comparing an implementation handoff with the candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImplementationCandidateOutcome {
    Valid,
    NoChange,
    ChangedFilesMismatch {
        claimed: Vec<String>,
        actual: Vec<String>,
    },
}

impl ImplementationCandidateOutcome {
    fn failure_code(&self) -> Option<&'static str> {
        match self {
            Self::Valid => None,
            Self::NoChange => Some("IMPLEMENTATION_NO_CHANGE"),
            Self::ChangedFilesMismatch { .. } => Some("IMPLEMENTATION_CHANGED_FILES_INVALID"),
        }
    }

    fn failure_message(&self) -> Option<String> {
        match self {
            Self::Valid => None,
            Self::NoChange => Some(
                "implementation completed without producing the required candidate mutation".into(),
            ),
            Self::ChangedFilesMismatch { claimed, actual } => Some(format!(
                "ImplementationHandoff.changed_files does not match the authoritative candidate; claimed={claimed:?}, actual={actual:?}"
            )),
        }
    }
}

pub fn validate_implementation_candidate(
    requirement: ImplementationMutationRequirement,
    input: &WorkspaceState,
    output: &WorkspaceState,
    handoff: &ImplementationHandoff,
    actual_changed_files: &[String],
) -> ImplementationCandidateOutcome {
    let claimed = handoff
        .changed_files
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let actual = actual_changed_files
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if claimed != actual || claimed.len() != handoff.changed_files.len() {
        return ImplementationCandidateOutcome::ChangedFilesMismatch {
            claimed: handoff.changed_files.clone(),
            actual: actual_changed_files.to_vec(),
        };
    }

    if requirement == ImplementationMutationRequirement::Required
        && (input.state_id == output.state_id || actual_changed_files.is_empty())
    {
        return ImplementationCandidateOutcome::NoChange;
    }

    ImplementationCandidateOutcome::Valid
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

#[path = "role_execution.rs"]
mod live_role_execution;

pub use live_role_execution::{RealAcpRoleExecutor, RoleAgentExecutor, RoleExecutionOutcome};

#[cfg(test)]
use live_role_execution::*;

/// Production Workflow Coordinator driving workflow runs to completion.
pub struct WorkflowCoordinator {
    pool: PgPool,
    credential_catalog_pool: PgPool,
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
            credential_catalog_pool: pool.clone(),
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

    /// Select accounts from an explicitly supplied catalog while workflow
    /// effects and evidence remain in this coordinator's control plane.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn with_credential_catalog(mut self, pool: PgPool) -> Self {
        self.credential_catalog_pool = pool;
        self
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

    #[allow(clippy::too_many_arguments)]
    async fn execute_role_with_operational_fallback(
        &self,
        workflow: &WorkflowRun,
        role_execution: &RoleExecution,
        role: &RoleDefinition,
        target: &ResolvedExecutionTarget,
        task_text: &str,
        repository: &Path,
        input_handoff: Option<&HandoffArtifact>,
        cancellation: tokio::sync::watch::Receiver<bool>,
    ) -> Result<RoleExecutionOutcome> {
        let initial_state = compute_workspace_state(
            repository,
            workflow.base_revision.as_deref().unwrap_or("HEAD"),
        )
        .await?;
        let first = self
            .executor
            .execute_role(
                &self.pool,
                workflow,
                role_execution,
                role,
                target,
                task_text,
                repository,
                input_handoff,
                cancellation.clone(),
            )
            .await;
        let error = match first {
            Ok(outcome) => return Ok(outcome),
            Err(error) => error,
        };
        let Some(failure) = error.downcast_ref::<live_role_execution::RoleOperationalFailure>()
        else {
            return Err(error);
        };
        if *cancellation.borrow() {
            return Err(error);
        }
        let current_role = self
            .store
            .get_role_execution(&role_execution.id)
            .await?
            .context("fallback role missing")?;
        ensure!(
            current_role.status == RoleExecutionStatus::Running
                && current_role.agent_execution_ids.last() == Some(&failure.agent_execution_id),
            "FALLBACK_EXECUTION_OWNER_MISMATCH"
        );
        let evidence: Option<(String, Option<String>, i64, i64, serde_json::Value)> = sqlx::query_as(
            "SELECT status, provider, turn_count, tool_call_count, metadata FROM orbit_agent_executions WHERE id = $1 AND role_execution_id = $2",
        ).bind(&failure.agent_execution_id).bind(&role_execution.id).fetch_optional(&self.pool).await?;
        let Some((status, provider, turns, calls, metadata)) = evidence else {
            return Err(UnconfirmedRoleCleanup("FALLBACK_EVIDENCE_UNCONFIRMED".into()).into());
        };
        if status != "FAILED"
            || provider.as_deref() != Some(target.provider.as_str())
            || turns != 0
            || calls != 0
            || !live_role_execution::safe_operational_failure(&metadata)
        {
            return Err(UnconfirmedRoleCleanup("FALLBACK_EVIDENCE_UNCONFIRMED".into()).into());
        }
        ensure!(
            compute_workspace_state(
                repository,
                workflow.base_revision.as_deref().unwrap_or("HEAD")
            )
            .await?
                == initial_state,
            "WORKSPACE_MUTATION_VIOLATION: pre-prompt fallback candidate changed"
        );
        let candidates = RoleRuntimeResolver::resolve_ranked_targets_live(
            &self.credential_catalog_pool,
            role,
            None,
            self.quota_selection_policy,
        )
        .await?;
        let Some(mut alternate) = candidates
            .into_iter()
            .find(|candidate| candidate.provider != target.provider)
        else {
            return Err(error);
        };
        alternate.resolution_reason.push_str(&format!(
            "; operational_fallback_from={}; workspace_state={}",
            failure.agent_execution_id, initial_state.state_id
        ));
        self.store
            .set_role_execution_resolved(&role_execution.id, &alternate)
            .await?;
        let continuing_role = self
            .store
            .get_role_execution(&role_execution.id)
            .await?
            .context("fallback role missing")?;
        // One alternate only. Semantic outcomes and uncertain provider effects
        // never enter this path, and an alternate failure cannot recurse.
        let mut outcome = self
            .executor
            .execute_role(
                &self.pool,
                workflow,
                &continuing_role,
                role,
                &alternate,
                task_text,
                repository,
                input_handoff,
                cancellation,
            )
            .await?;
        outcome
            .agent_execution_ids
            .insert(0, failure.agent_execution_id.clone());
        Ok(outcome)
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
            credential_catalog_pool: self.credential_catalog_pool.clone(),
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
                // Read-only completion must bind its handoff to the candidate
                // observed before reasoning, then prove that candidate unchanged.
                let initial_state = if self
                    .store
                    .flow(wf_id)
                    .await?
                    .is_some_and(|flow| flow.read_only)
                {
                    Some(
                        compute_workspace_state(
                            workflow_repo_path(&wf)?,
                            wf.base_revision.as_deref().unwrap_or("HEAD"),
                        )
                        .await?,
                    )
                } else {
                    None
                };
                if let (Some(expected), Some(observed)) = (
                    wf.current_workspace_state_id.as_deref(),
                    initial_state.as_ref(),
                ) {
                    ensure!(expected == observed.state_id, "STALE_CANDIDATE");
                }
                self.store
                    .transition_workflow_stage(
                        wf_id,
                        WorkflowStage::Planning,
                        initial_state.as_ref().map(|state| state.state_id.as_str()),
                        None,
                        None,
                    )
                    .await?;
                Ok(WorkflowStepResult::Advanced {
                    from: WorkflowStage::Created,
                    to: WorkflowStage::Planning,
                })
            }

            WorkflowStage::Planning => {
                let role = if let Some(preferences) =
                    crate::interactive::preferences::turn_preferences(&self.pool, wf_id).await?
                {
                    ensure!(
                        self.store
                            .flow(wf_id)
                            .await?
                            .is_some_and(|flow| flow.read_only),
                        "ORCHESTRATOR_REQUIRES_READ_ONLY_FLOW"
                    );
                    preferences.orchestrator_role()?
                } else {
                    RoleDefinition::planner_v1()
                };
                let target = RoleRuntimeResolver::resolve_target_live_with_policy(
                    &self.credential_catalog_pool,
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
                    .execute_role_with_operational_fallback(
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

                let read_only = self
                    .store
                    .flow(wf_id)
                    .await?
                    .is_some_and(|flow| flow.read_only);
                let plan: PlanHandoff = match extract_structured_envelope::<PlanHandoff>(
                    &outcome.raw_output,
                    "PlanHandoff",
                ) {
                    Ok(p) => {
                        let validation = if read_only {
                            p.validate_read_only()
                        } else {
                            p.validate()
                        };
                        if let Err(e) = validation {
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

                if role.role_id == "orchestrator"
                    && let Err(error) = crate::interactive::intent::record_proposal(
                        &self.pool,
                        wf_id,
                        &outcome.raw_output,
                        None,
                    )
                    .await
                {
                    let message = format!("ROLE_OUTPUT_INVALID: intent proposal: {error:#}");
                    self.store
                        .complete_role_execution_failed(
                            &role_exec.id,
                            "ROLE_OUTPUT_INVALID",
                            &message,
                        )
                        .await?;
                    self.store
                        .transition_workflow_stage(
                            wf_id,
                            WorkflowStage::Failed,
                            None,
                            None,
                            Some(&message),
                        )
                        .await?;
                    return Ok(WorkflowStepResult::Terminal(WorkflowStage::Failed));
                }

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

                if read_only {
                    let state_id = wf
                        .current_workspace_state_id
                        .as_deref()
                        .context("read-only flow candidate missing")?;
                    require_candidate_state(&wf, state_id).await?;
                    self.store
                        .transition_workflow_stage(
                            wf_id,
                            WorkflowStage::Completed,
                            Some(state_id),
                            None,
                            None,
                        )
                        .await?;
                    return Ok(WorkflowStepResult::Terminal(WorkflowStage::Completed));
                }

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
                    &self.credential_catalog_pool,
                    &role,
                    None,
                    self.quota_selection_policy,
                )
                .await?;

                let plan_handoff = self
                    .store
                    .get_latest_handoff_of_type(wf_id, HandoffType::Plan)
                    .await?;
                let repo_path = workflow_repo_path(&wf)?;
                let baseline = wf.base_revision.as_deref().unwrap_or("HEAD");
                let input_ws_state = compute_workspace_state(repo_path, baseline).await?;

                let role_exec = self
                    .store
                    .create_role_execution(
                        wf_id,
                        &role,
                        "IMPLEMENTING",
                        wf.iteration,
                        Some(&input_ws_state.state_id),
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
                    .execute_role_with_operational_fallback(
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

                let new_ws_state = compute_workspace_state(repo_path, baseline).await?;
                let actual_changed_files = candidate_changed_files(repo_path, baseline).await?;
                let candidate_outcome = validate_implementation_candidate(
                    ImplementationMutationRequirement::Required,
                    &input_ws_state,
                    &new_ws_state,
                    &impl_handoff,
                    &actual_changed_files,
                );

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

                if let Some(failure_code) = candidate_outcome.failure_code() {
                    let failure_message = candidate_outcome
                        .failure_message()
                        .expect("failed candidate outcome has a message");
                    self.store
                        .fail_mutating_role_and_workflow(
                            &role_exec.id,
                            wf_id,
                            &wf.attempt_id,
                            &new_ws_state.state_id,
                            failure_code,
                            &failure_message,
                            &handoff.id,
                        )
                        .await?;
                    return Ok(WorkflowStepResult::Terminal(WorkflowStage::Failed));
                }

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

                let review_tier = if let Some(flow) = self.store.flow(wf_id).await? {
                    let paths = candidate_changed_files(
                        workflow_repo_path(&wf)?,
                        wf.base_revision.as_deref().unwrap_or("HEAD"),
                    )
                    .await?;
                    flow.effective_tiers(&paths).0.max(
                        policies
                            .regression
                            .as_ref()
                            .map_or(VerificationTier::Fast, |policy| policy.review_gate_tier),
                    )
                } else {
                    VerificationTier::Standard
                };
                if review_tier > VerificationTier::Fast {
                    let std_run = self
                        .run_tier_verification(
                            &wf,
                            &ws_state,
                            review_tier,
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
                }

                // All required feedback gates passed on the current candidate.
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
                    &self.credential_catalog_pool,
                    &role,
                    None,
                    self.quota_selection_policy,
                )
                .await?;

                let failure_handoff = self
                    .store
                    .get_latest_handoff_of_type(wf_id, HandoffType::FailureEvidence)
                    .await?;
                let repo_path = workflow_repo_path(&wf)?;
                let baseline = wf.base_revision.as_deref().unwrap_or("HEAD");
                let input_ws_state =
                    if let Some(state_id) = wf.current_workspace_state_id.as_deref() {
                        require_candidate_state(&wf, state_id).await?
                    } else {
                        compute_workspace_state(repo_path, baseline).await?
                    };

                let role_exec = self
                    .store
                    .create_role_execution(
                        wf_id,
                        &role,
                        "REPAIRING",
                        next_iteration,
                        Some(&input_ws_state.state_id),
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
                    .execute_role_with_operational_fallback(
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

                let new_ws_state = compute_workspace_state(repo_path, baseline).await?;
                let actual_changed_files = candidate_changed_files(repo_path, baseline).await?;
                let candidate_outcome = validate_implementation_candidate(
                    ImplementationMutationRequirement::Required,
                    &input_ws_state,
                    &new_ws_state,
                    &impl_handoff,
                    &actual_changed_files,
                );

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

                if let Some(failure_code) = candidate_outcome.failure_code() {
                    let failure_message = candidate_outcome
                        .failure_message()
                        .expect("failed candidate outcome has a message");
                    self.store
                        .fail_mutating_role_and_workflow(
                            &role_exec.id,
                            wf_id,
                            &wf.attempt_id,
                            &new_ws_state.state_id,
                            failure_code,
                            &failure_message,
                            &handoff.id,
                        )
                        .await?;
                    return Ok(WorkflowStepResult::Terminal(WorkflowStage::Failed));
                }

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
                    &self.credential_catalog_pool,
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
                    .execute_role_with_operational_fallback(
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

                let completion_tier = if let Some(flow) = self.store.flow(wf_id).await? {
                    let paths = candidate_changed_files(repo_path, baseline).await?;
                    flow.effective_tiers(&paths).1.max(
                        policies
                            .regression
                            .as_ref()
                            .map_or(VerificationTier::Fast, |policy| policy.completion_tier),
                    )
                } else {
                    VerificationTier::Full
                };
                let full_run = self
                    .run_tier_verification(
                        &wf,
                        &ws_state,
                        completion_tier,
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
                self.store
                    .check_technical_completion_invariant(wf_id)
                    .await?;
                if crate::workflow::reasoning::ReasoningStore::new(self.pool.clone())
                    .requires_acceptance(wf_id)
                    .await?
                {
                    self.store
                        .transition_workflow_stage(
                            wf_id,
                            WorkflowStage::BusinessAcceptance,
                            Some(ws_state_id),
                            None,
                            None,
                        )
                        .await?;
                    return Ok(WorkflowStepResult::Advanced {
                        from: WorkflowStage::Regression,
                        to: WorkflowStage::BusinessAcceptance,
                    });
                }

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

            WorkflowStage::BusinessAcceptance => {
                let ws = wf
                    .current_workspace_state_id
                    .as_deref()
                    .context("acceptance candidate missing")?;
                let repo_path = Path::new(
                    wf.repository_path
                        .as_deref()
                        .context("acceptance repository missing")?,
                );
                let baseline = wf
                    .base_revision
                    .as_deref()
                    .context("acceptance baseline missing")?;
                if let Err(error) =
                    crate::workflow::reasoning::ReasoningStore::new(self.pool.clone())
                        .check_acceptance(wf_id, ws)
                        .await
                {
                    if error.is::<crate::workflow::reasoning::BusinessAcceptanceRequired>() {
                        return Ok(WorkflowStepResult::Waiting);
                    }
                    return Err(error);
                }
                ensure!(
                    compute_workspace_state(repo_path, baseline).await?.state_id == ws,
                    "STALE_ACCEPTANCE_CANDIDATE"
                );
                self.store.check_completion_invariant(wf_id).await?;
                self.store
                    .transition_workflow_stage(
                        wf_id,
                        WorkflowStage::Completed,
                        Some(ws),
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
        let current_repair_failure = if wf.status == WorkflowStage::Repairing {
            self.store
                .get_latest_handoff_of_type(&wf.id, HandoffType::FailureEvidence)
                .await?
        } else {
            None
        };
        let Some(role) = roles.iter().rev().find(|role| {
            role.stage == wf.status.as_str()
                && role.iteration == wf.iteration
                && role.status == RoleExecutionStatus::Succeeded
                && role.handoff_output_id.is_some()
                && (wf.status != WorkflowStage::Repairing
                    || (role.handoff_input_id.as_deref()
                        == current_repair_failure
                            .as_ref()
                            .map(|handoff| handoff.id.as_str())
                        && role.input_workspace_state_id.as_deref()
                            == wf.current_workspace_state_id.as_deref()))
        }) else {
            return Ok(None);
        };
        let next = match wf.status {
            WorkflowStage::Planning => {
                if self
                    .store
                    .flow(&wf.id)
                    .await?
                    .is_some_and(|flow| flow.read_only)
                {
                    let state_id = wf
                        .current_workspace_state_id
                        .as_deref()
                        .context("read-only candidate missing")?;
                    require_candidate_state(wf, state_id).await?;
                    WorkflowStage::Completed
                } else {
                    WorkflowStage::Implementing
                }
            }
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
            let changed_files =
                candidate_changed_files(repo_path, wf.base_revision.as_deref().unwrap_or("HEAD"))
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

pub(crate) async fn candidate_changed_files(
    repo_path: &Path,
    baseline: &str,
) -> Result<Vec<String>> {
    if !repo_path.join(".git").exists() {
        return non_git_candidate_paths(repo_path)?
            .into_iter()
            .map(|path| {
                path.to_str()
                    .context("candidate path is not UTF-8")
                    .map(str::to_owned)
            })
            .collect();
    }
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
    let output = crate::execution::process::bounded_output(
        crate::tool_surface::safe_git_command(repo_path, args),
        32 * 1024 * 1024,
        std::time::Duration::from_secs(30),
    )
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

pub(crate) async fn review_candidate_diff(
    repo_path: &Path,
    baseline_revision: &str,
) -> Result<String> {
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

        exec
    }
}

#[async_trait::async_trait]
impl RoleAgentExecutor for SimulatedRoleExecutor {
    async fn execute_role(
        &self,
        _pool: &PgPool,
        wf_run: &WorkflowRun,
        role_exec: &RoleExecution,
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
            "planner" | "orchestrator" => {
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

                let response = if role_exec.stage == "REPAIRING" {
                    self.repair_response.lock().unwrap().clone()
                } else {
                    self.impl_response.lock().unwrap().clone()
                };
                match response {
                    Some(response) => response,
                    None => {
                        let changed_files = candidate_changed_files(
                            repo_path,
                            wf_run.base_revision.as_deref().unwrap_or("HEAD"),
                        )
                        .await?;
                        format!(
                            "{}\n{}\n{}",
                            ORBIT_HANDOFF_START,
                            serde_json::to_string(&ImplementationHandoff {
                                summary: if role_exec.stage == "REPAIRING" {
                                    "Default repair implementation".into()
                                } else {
                                    "Default implementation".into()
                                },
                                changed_files,
                                tests_added_or_modified: vec![],
                                exploratory_commands: vec![],
                                known_limitations: vec![],
                                verification_notes: vec![],
                            })
                            .unwrap(),
                            ORBIT_HANDOFF_END
                        )
                    }
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

        let raw_output = if role.role_id == "orchestrator"
            && !raw_output.contains("<<<ORBIT_INTENT_START>>>")
        {
            format!(
                "<<<ORBIT_INTENT_START>>>\n{{\"skill\":\"explain\",\"proposed_flow\":\"investigation\",\"rationale\":\"Simulated read-only observation\",\"scope\":[],\"clarification_questions\":[]}}\n<<<ORBIT_INTENT_END>>>\n{raw_output}"
            )
        } else {
            raw_output
        };
        Ok(RoleExecutionOutcome {
            raw_output,
            agent_execution_ids: vec![format!("sim-agent-{}", id())],
            termination_reason: Some("completed".into()),
        })
    }
}

pub struct AcpTurnState<'a> {
    pub repo_path: &'a Path,
    pub workspace_access: WorkspaceAccess,
    pub role_id: Option<String>,
    pub workspace_identity: Option<String>,
    pub tool_call_limit: u64,
    pub execution_profile: crate::execution::local::RoleExecutionProfile,
    pub role_budget: Option<crate::tools::budget::RoleBudget>,
    pub role_usage: crate::tools::budget::RoleUsage,
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
            execution_profile: Default::default(),
            role_budget: None,
            role_usage: Default::default(),
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

const TOOL_CALL_AUDIT_LIMIT: usize = 1024;
const PROVIDER_TOOL_NAME_QUEUE_LIMIT: usize = 1024;
const TOOL_PATH_INPUT_LIMIT: usize = 4096;
const TOOL_PATH_DISPLAY_LIMIT: usize = 192;
const READ_FILE_CONTINUATION_RESERVE_BYTES: usize = 256;

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

    #[expect(
        clippy::too_many_arguments,
        reason = "audit identity and counter inputs stay explicit at dispatch"
    )]
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
            && let Some(invocation) = self.provider_tool_invocations.get_mut(&meta.invocation_id)
        {
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

    fn operation_error_code(&self, sequence: u64) -> Option<&'static str> {
        self.active_call
            .as_ref()
            .filter(|active| active.sequence == sequence)
            .and_then(|active| active.operation_error_code)
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
            let mut audit = state.tool_call_audit.metadata(
                state.tool_calls,
                state.tool_successes,
                state.tool_failures,
            );
            audit["role_budget"] =
                serde_json::json!({"limits":state.role_budget,"usage":state.role_usage});
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
    if method == "orbit/shell" {
        return Some(ProviderToolMethodObservation {
            provider_tool_name: "orbit_shell",
            request_tool_name: "shell",
            canonical_tool_name: Tool::TerminalCreate,
        });
    }
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
        "TOOL_BUDGET_EXHAUSTED",
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
    let required_args_valid = required.iter().all(|(field, nonempty)| {
        params
            .get(*field)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !*nonempty || !value.is_empty())
    });
    let read_range_valid = tool != Tool::FsReadTextFile
        || (params.get("line").is_none_or(|line| {
            line.as_u64()
                .is_some_and(|line| (1..=u32::MAX as u64).contains(&line))
        }) && params.get("limit").is_none_or(|limit| {
            limit
                .as_u64()
                .is_some_and(|limit| (1..=u32::MAX as u64).contains(&limit))
        }));
    required_args_valid && read_range_valid
}

fn requested_text_read_range(params: &serde_json::Value) -> Result<(u32, Option<u32>)> {
    let line = params
        .get("line")
        .map(|line| {
            line.as_u64()
                .context("INVALID_REQUEST: file read line must be a positive integer")
                .and_then(|line| {
                    u32::try_from(line)
                        .context("INVALID_REQUEST: file read line is outside the supported range")
                })
        })
        .transpose()?
        .unwrap_or(1);
    ensure!(line > 0, "INVALID_REQUEST: file read line must be positive");
    let limit = params
        .get("limit")
        .map(|limit| {
            limit
                .as_u64()
                .context("INVALID_REQUEST: file read limit must be a positive integer")
                .and_then(|limit| {
                    u32::try_from(limit)
                        .context("INVALID_REQUEST: file read limit is outside the supported range")
                })
        })
        .transpose()?;
    ensure!(
        limit.is_none_or(|limit| limit > 0),
        "INVALID_REQUEST: file read limit must be positive"
    );
    Ok((line, limit))
}

fn text_read_window(content: &str, start_line: u32, line_limit: Option<u32>) -> (usize, usize) {
    let start = if start_line == 1 {
        0
    } else {
        let mut current_line = 1u32;
        content
            .match_indices('\n')
            .find_map(|(index, _)| {
                current_line = current_line.saturating_add(1);
                (current_line == start_line).then_some(index + 1)
            })
            .unwrap_or(content.len())
    };
    if start == content.len() {
        return (start, start);
    }

    let Some(line_limit) = line_limit else {
        return (start, content.len());
    };
    let mut lines_seen = 1u32;
    for (relative_index, byte) in content[start..].bytes().enumerate() {
        if byte == b'\n' {
            if lines_seen == line_limit {
                return (start, start + relative_index + 1);
            }
            lines_seen = lines_seen.saturating_add(1);
        }
    }
    (start, content.len())
}

fn text_line_count(content: &str) -> u32 {
    if content.is_empty() {
        0
    } else {
        let newlines = content.bytes().filter(|byte| *byte == b'\n').count() as u32;
        newlines.saturating_add(u32::from(!content.ends_with('\n')))
    }
}

fn bounded_text_read_result(
    content: &str,
    start_line: u32,
    line_limit: Option<u32>,
    output_limit: usize,
) -> Result<serde_json::Value> {
    let (start, window_end) = text_read_window(content, start_line, line_limit);
    let window = &content[start..window_end];
    let mut low = 0usize;
    let mut high = window.len();
    let mut best = None;
    while low <= high {
        let midpoint = low + (high - low) / 2;
        let end = if midpoint == window.len() {
            midpoint
        } else {
            window.as_bytes()[..midpoint]
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |index| index + 1)
        };
        if end == 0 && low == 0 && !window.is_empty() {
            low = window
                .as_bytes()
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(window.len(), |index| index + 1);
            continue;
        }
        if end < low {
            if low >= window.len() {
                break;
            }
            let next_boundary = window.as_bytes()[low..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(window.len(), |index| low + index + 1);
            if next_boundary <= low {
                break;
            }
            low = next_boundary;
            continue;
        }
        let next_byte = start + end;
        let truncated = next_byte < content.len();
        let page = &window[..end];
        let next_line = start_line.saturating_add(text_line_count(page));
        let result = serde_json::json!({
            "content": page,
            "_meta": {
                "orbit": {
                    "line": start_line,
                    "total_bytes": content.len(),
                    "total_size": content.len(),
                    "bytes_returned": page.len(),
                    "truncated": truncated,
                    "next_offset": truncated.then_some(next_byte),
                    "next_line": truncated.then_some(next_line),
                }
            },
        });
        if (end > 0 || window.is_empty()) && serde_json::to_vec(&result)?.len() <= output_limit {
            best = Some(result);
            if end == window.len() {
                break;
            }
            low = end + 1;
        } else {
            if end == 0 {
                break;
            }
            high = end - 1;
        }
    }

    best.context("OUTPUT_LIMIT: no complete text line fits in the response limit")
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

    if entries.len() > 64 {
        report.push_str(&format!(
            "Showing the latest 64 of {} retained callback rows.\n",
            entries.len()
        ));
    }
    for entry in entries.iter().skip(entries.len().saturating_sub(64)) {
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
    let output_before = wire.response_payload_bytes();
    if persist_audit {
        let remaining = state.role_budget.as_ref().map_or(65536, |budget| {
            budget
                .max_output_bytes
                .saturating_sub(state.role_usage.output_bytes)
                .saturating_sub(128) as usize
        });
        wire.set_response_limit(remaining.min(65536));
    }
    let mut result = handle_acp_message_inner(wire, state, message).await;
    if persist_audit {
        state.role_usage.output_bytes = state
            .role_usage
            .output_bytes
            .saturating_add(wire.response_payload_bytes().saturating_sub(output_before));
    }
    if result
        .as_ref()
        .err()
        .is_some_and(|error| error.is::<crate::tools::budget::ToolBudgetExhausted>())
    {
        state.role_usage.exhausted = true;
    }
    if state.role_usage.exhausted && result.is_ok() {
        result = Err(crate::tools::budget::ToolBudgetExhausted.into());
    }
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
        if let Some(budget) = &state.role_budget {
            // Denied effects still consume callback capacity, but cannot spend
            // mutation or terminal allowances that the role does not possess.
            let mutating = canonical
                .map(crate::tool_surface::ToolMetadata::for_tool)
                .is_some_and(|metadata| {
                    metadata.mutating
                        && state.workspace_access == WorkspaceAccess::ReadWrite
                        && state
                            .role_id
                            .as_deref()
                            .is_some_and(|role| metadata.is_role_allowed(role))
                });
            if let Err(error) = budget.reserve_call(
                &mut state.role_usage,
                mutating,
                mutating
                    && canonical == Some(crate::tool_surface::CanonicalToolName::TerminalCreate),
            ) {
                state.tool_failures += 1;
                state.tool_call_audit.finish_call(
                    audit_sequence,
                    ToolCallOutcome::ExpectedDenial,
                    Some("TOOL_BUDGET_EXHAUSTED"),
                );
                wire.set_response_limit(128);
                wire.response_error(req_id, -32603, "TOOL_BUDGET_EXHAUSTED")
                    .await?;
                return Err(error);
            }
        }

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
            state.execution_profile,
            crate::execution::local::RoleExecutionProfile::Trusted
        ) && matches!(
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

        let mut meta = match crate::tool_surface::authorize_repository_tool(
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
        if let Some(budget) = &state.role_budget {
            meta.max_output_bytes = meta.max_output_bytes.min(
                budget
                    .max_output_bytes
                    .saturating_sub(state.role_usage.output_bytes)
                    .saturating_sub(128) as usize,
            );
        }
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
                    let remaining_read =
                        state
                            .role_budget
                            .as_ref()
                            .map_or(64 * 1024 * 1024, |budget| {
                                budget
                                    .max_file_read_bytes
                                    .saturating_sub(state.role_usage.file_read_bytes)
                            });
                    if let Some(offset) = params.get("offset") {
                        let offset = offset
                            .as_u64()
                            .context("INVALID_REQUEST: invalid byte offset")?;
                        ensure!(
                            params.get("line").is_none() && params.get("limit").is_none(),
                            "INVALID_REQUEST: mixed line and byte ranges"
                        );
                        let max_bytes = params
                            .get("max_bytes")
                            .map(|value| {
                                value.as_u64().context("INVALID_REQUEST: invalid byte size")
                            })
                            .transpose()?
                            .unwrap_or(8192);
                        ensure!(
                            (4..=65536).contains(&max_bytes),
                            "INVALID_REQUEST: invalid byte size"
                        );
                        if remaining_read < 4 {
                            return Err(crate::tools::budget::ToolBudgetExhausted.into());
                        }
                        let read_size = max_bytes
                            .min(remaining_read)
                            .min(meta.max_output_bytes.saturating_sub(1024) as u64 / 6);
                        if read_size < 4 {
                            return Err(crate::tools::budget::ToolBudgetExhausted.into());
                        }
                        state.role_usage.file_read_bytes += read_size;
                        let (content, total_size, bytes_read) =
                            crate::fs_tools::read_text_byte_range(
                                state.repo_path,
                                rel_path_str,
                                offset,
                                read_size as usize,
                            )?;
                        state.role_usage.file_read_bytes -=
                            read_size.saturating_sub(bytes_read as u64);
                        let next_offset = offset + content.len() as u64;
                        state.tool_successes += 1;
                        wire.response_ok(req_id, serde_json::json!({"content":content,"_meta":{"orbit":{"bytes_returned":content.len(),"total_size":total_size,"total_bytes":total_size,"truncated":next_offset < total_size,"next_offset":(next_offset < total_size).then_some(next_offset)}}})).await?;
                        return Ok(());
                    }
                    let (start_line, line_limit) = match requested_text_read_range(&params) {
                        Ok(range) => range,
                        Err(error) => {
                            state
                                .tool_call_audit
                                .record_operation_error(audit_sequence, &error);
                            state.tool_failures = state.tool_failures.saturating_add(1);
                            wire.response_error(req_id, -32602, "INVALID_REQUEST")
                                .await?;
                            return Ok(());
                        }
                    };
                    match crate::fs_tools::read_text_confined_bounded(
                        state.repo_path,
                        rel_path_str,
                        remaining_read,
                    ) {
                        Ok(content) => {
                            state.role_usage.file_read_bytes += content.len() as u64;
                            match bounded_text_read_result(
                                &content,
                                start_line,
                                line_limit,
                                meta.max_output_bytes
                                    .saturating_sub(READ_FILE_CONTINUATION_RESERVE_BYTES),
                            ) {
                                Ok(result) => {
                                    state.tool_successes += 1;
                                    wire.response_ok(req_id, result).await?;
                                }
                                Err(error) => {
                                    state
                                        .tool_call_audit
                                        .record_operation_error(audit_sequence, &error);
                                    state.tool_failures = state.tool_failures.saturating_add(1);
                                    let invalid_offset =
                                        normalized_tool_error_code(&error) == "INVALID_REQUEST";
                                    if invalid_offset {
                                        wire.response_error(req_id, -32602, "INVALID_REQUEST")
                                            .await?;
                                    } else {
                                        wire.response_error(
                                            req_id,
                                            -32603,
                                            crate::tool_surface::ERR_OUTPUT_LIMIT,
                                        )
                                        .await?;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            if e.downcast_ref::<std::string::FromUtf8Error>().is_some() {
                                state.role_usage.file_read_bytes += remaining_read;
                            }
                            if e.is::<crate::tools::budget::ToolBudgetExhausted>() {
                                return Err(e);
                            }
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
                    match crate::tool_surface::AgentTerminal::spawn_confined(
                        &state.execution_profile,
                        state.repo_path,
                        &cwd,
                        &cmd_bin,
                        &cmd_args,
                        output_byte_limit,
                    ) {
                        Ok(term) => {
                            if method == "orbit/shell" {
                                let term = Arc::new(term);
                                let tid = format!("term-{}", crate::model::id());
                                state.terminals.insert(tid.clone(), term.clone());
                                let result = term.wait_for_exit(Duration::from_secs(300)).await;
                                term.kill().await?;
                                state.terminals.remove(&tid);
                                match result {
                                    Ok(code) => {
                                        state.tool_successes += 1;
                                        let output = term.output();
                                        wire.response_ok(req_id, serde_json::json!({"exit_code": code, "output": output.text(), "truncated": output.truncated})).await?;
                                    }
                                    Err(error) => {
                                        state
                                            .tool_call_audit
                                            .record_operation_error(audit_sequence, &error);
                                        state.tool_failures += 1;
                                        wire.response_error(req_id, -32603, "COMMAND_TIMEOUT")
                                            .await?;
                                    }
                                }
                                return Ok(());
                            }
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
                if error.is::<crate::tools::budget::ToolBudgetExhausted>() {
                    state.role_usage.exhausted = true;
                    state.tool_failures += 1;
                    state.tool_call_audit.finish_call(
                        audit_sequence,
                        ToolCallOutcome::ExpectedDenial,
                        Some("TOOL_BUDGET_EXHAUSTED"),
                    );
                    wire.response_error(timeout_request_id, -32603, "TOOL_BUDGET_EXHAUSTED")
                        .await?;
                    return Err(error);
                }
                if matches!(
                    normalized_tool_error_code(&error),
                    "INVALID_REQUEST" | "PATH_NOT_FOUND" | "PATH_OUTSIDE_WORKSPACE"
                ) {
                    let code = normalized_tool_error_code(&error);
                    state.tool_failures += 1;
                    state.tool_call_audit.finish_call(
                        audit_sequence,
                        if code == "INVALID_REQUEST" {
                            ToolCallOutcome::InvalidRequest
                        } else {
                            ToolCallOutcome::ExecutionFailure
                        },
                        Some(code),
                    );
                    wire.response_error(timeout_request_id, -32602, code)
                        .await?;
                    return Ok(());
                }
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
        } else if !tool_request_has_valid_required_args(tool, &params)
            || state.tool_call_audit.operation_error_code(audit_sequence) == Some("INVALID_REQUEST")
        {
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn implementation_handoff(changed_files: &[&str]) -> ImplementationHandoff {
        ImplementationHandoff {
            summary: "implemented candidate".into(),
            changed_files: changed_files.iter().map(|path| (*path).into()).collect(),
            tests_added_or_modified: vec![],
            exploratory_commands: vec![],
            known_limitations: vec![],
            verification_notes: vec![],
        }
    }

    #[test]
    fn implementation_candidate_requires_authoritative_mutation_and_exact_claims() {
        let input = WorkspaceState::compute_candidate_v2("base", "head", "input");
        let output = WorkspaceState::compute_candidate_v2("base", "head", "output");

        assert_eq!(
            validate_implementation_candidate(
                ImplementationMutationRequirement::Required,
                &input,
                &output,
                &implementation_handoff(&["src/a.rs", "src/b.rs"]),
                &["src/a.rs".into(), "src/b.rs".into()],
            ),
            ImplementationCandidateOutcome::Valid
        );
        assert_eq!(
            validate_implementation_candidate(
                ImplementationMutationRequirement::Required,
                &input,
                &input,
                &implementation_handoff(&[]),
                &[],
            ),
            ImplementationCandidateOutcome::NoChange
        );
        assert!(matches!(
            validate_implementation_candidate(
                ImplementationMutationRequirement::Required,
                &input,
                &input,
                &implementation_handoff(&["src/a.rs"]),
                &[],
            ),
            ImplementationCandidateOutcome::ChangedFilesMismatch { .. }
        ));
        assert!(matches!(
            validate_implementation_candidate(
                ImplementationMutationRequirement::Required,
                &input,
                &output,
                &implementation_handoff(&["src/a.rs"]),
                &["src/a.rs".into(), "src/b.rs".into()],
            ),
            ImplementationCandidateOutcome::ChangedFilesMismatch { .. }
        ));
        assert!(matches!(
            validate_implementation_candidate(
                ImplementationMutationRequirement::Required,
                &input,
                &output,
                &implementation_handoff(&["src/a.rs", "src/a.rs"]),
                &["src/a.rs".into()],
            ),
            ImplementationCandidateOutcome::ChangedFilesMismatch { .. }
        ));
    }

    #[test]
    fn implementation_candidate_can_explicitly_allow_no_change() {
        let state = WorkspaceState::compute_candidate_v2("base", "head", "same");
        assert_eq!(
            validate_implementation_candidate(
                ImplementationMutationRequirement::NoChangeAllowed,
                &state,
                &state,
                &implementation_handoff(&[]),
                &[],
            ),
            ImplementationCandidateOutcome::Valid
        );
    }

    #[test]
    fn bounded_text_read_searches_the_first_complete_line_boundary() -> Result<()> {
        let short = bounded_text_read_result("a\n", 1, None, 65_280)?;
        assert_eq!(short["content"], "a\n");
        assert_eq!(short["_meta"]["orbit"]["truncated"], false);

        let content = format!("{}\n{}\n", "x".repeat(800), "y".repeat(500));
        let first_page = bounded_text_read_result(&content, 1, None, 1_024)?;
        assert_eq!(first_page["content"], format!("{}\n", "x".repeat(800)));
        assert_eq!(first_page["_meta"]["orbit"]["truncated"], true);
        assert_eq!(first_page["_meta"]["orbit"]["next_line"], 2);
        Ok(())
    }

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
        let read_count = TOOL_CALL_AUDIT_LIMIT as u64 - 2;
        for sequence in 1..=read_count {
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
        let denied = audit.begin_call(
            read_count + 1,
            "fs/write_text_file",
            Some(Tool::FsWriteTextFile),
            read_count,
            0,
        );
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
        audit.set_turn_completion(true, read_count + 1);

        let entries = &audit.entries;
        assert_eq!(entries.len(), TOOL_CALL_AUDIT_LIMIT);
        assert_eq!(
            entries[TOOL_CALL_AUDIT_LIMIT - 2].advertised_to_provider,
            Some(false)
        );
        assert_eq!(entries[TOOL_CALL_AUDIT_LIMIT - 2].role_allowed, Some(false));
        assert_eq!(
            entries[TOOL_CALL_AUDIT_LIMIT - 2].mutation_applied,
            Some(false)
        );
        assert_eq!(
            entries[TOOL_CALL_AUDIT_LIMIT - 1].provider_tool_name,
            "unknown"
        );
        assert_eq!(
            entries[TOOL_CALL_AUDIT_LIMIT - 1].provider_name_mapping,
            "UNMATCHED"
        );
        assert_eq!(
            entries[TOOL_CALL_AUDIT_LIMIT - 1].advertised_to_provider,
            None
        );
        assert_eq!(entries[TOOL_CALL_AUDIT_LIMIT - 1].role_allowed, None);
        assert_eq!(
            entries[TOOL_CALL_AUDIT_LIMIT - 1].error_code,
            Some("PROVIDER_CALLBACK_UNRESOLVED")
        );
        assert_eq!(audit.omitted_count, 2);
        assert_eq!(audit.mutating_count, 1);
        assert_eq!(audit.mutating_unknown_count, 3);
        assert_eq!(audit.denied_count, 1);

        let metadata = serde_json::json!({
            "tool_call_audit": audit.metadata(read_count + 1, read_count, 1)
        });
        let summary = &metadata["tool_call_audit"]["summary"];
        assert_eq!(summary["total"], TOOL_CALL_AUDIT_LIMIT as u64 + 2);
        assert_eq!(summary["successful"], read_count);
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
        let source = include_str!("coordinator.rs")
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
}
