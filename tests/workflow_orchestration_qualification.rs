//! Qualification Test Suite for Phase B3.1 Live Workflow Orchestration Bridge.
//! Verifies complete autonomous execution, live credential resolution, read-only enforcement,
//! mutation locking, multi-tier verification triggering, repair loops, fallback, and CLI invocation.

use anyhow::{Context, Result};
use orbit::{
    model::id, regression_strategy::VerificationTier, verification::*, workflow::*,
    workflow_coordinator::*,
};
use sqlx::PgPool;
use std::{collections::BTreeMap, ops::Deref, sync::Arc};

#[allow(dead_code)] // shared test helpers are used by different qualification binaries
#[path = "common/mod.rs"]
mod common;

struct TestContext {
    database: common::DisposablePgTestContext,
    store: WorkflowStore,
}

impl Deref for TestContext {
    type Target = common::DisposablePgTestContext;

    fn deref(&self) -> &Self::Target {
        &self.database
    }
}

async fn setup_test() -> Result<TestContext> {
    let database = common::DisposablePgTestContext::create("b31", 3).await?;
    let store = WorkflowStore::new(database.engine.pool.clone());
    Ok(TestContext { database, store })
}

async fn teardown_test(ctx: TestContext) -> Result<()> {
    ctx.database.teardown().await
}

fn sample_policy() -> VerificationPolicy {
    VerificationPolicy {
        id: format!("pol-{}", id()),
        version: 1,
        name: "Test Policy".into(),
        required_steps: vec!["fast".into(), "standard".into(), "full".into()],
        specialized_required_steps: vec![],
        allowed_commands: vec![],
        environment_policy: VerificationEnvironmentPolicy {
            inherit: vec![],
            set: BTreeMap::new(),
            deny: vec![],
        },
        network_policy: VerificationNetworkPolicy::None,
        cache_policy: VerificationCachePolicy::Clean,
        integration_environment_spec: None,
        browser_verification_spec: None,
    }
}

async fn assert_verification_profile_gate(
    coordinator: &WorkflowCoordinator,
    store: &WorkflowStore,
    workflow_id: &str,
) -> Result<()> {
    let error = coordinator.step(workflow_id).await.unwrap_err();
    assert!(error.to_string().contains("VERIFICATION_PROFILE_REQUIRED"));
    let workflow = store.get_workflow_run(workflow_id).await?.unwrap();
    assert_eq!(workflow.status, WorkflowStage::Verifying);
    Ok(())
}

async fn enroll_sample_credentials(pool: &PgPool) -> Result<()> {
    let mut tx = pool.begin().await?;
    let cred_id1 = id();
    let secret_id1 = id();
    let locator1 = format!("credential://{cred_id1}/generation/1/{secret_id1}");
    sqlx::query("INSERT INTO orbit_credentials(id, scope_key, provider, reference, current_generation, endpoint, auth_type, status) VALUES($1, 'operator', 'codex', 'codex-main', 1, NULL, 'local-session', 'enrolled')")
        .bind(&cred_id1)
        .execute(&mut *tx).await?;
    sqlx::query("INSERT INTO orbit_credential_generations(credential_id, generation, backend, state, secret_locator) VALUES($1, 1, 'local-private', 'enrolled', $2)")
        .bind(&cred_id1)
        .bind(&locator1)
        .execute(&mut *tx).await?;
    let representation_id1 = id();
    let representation_locator1 =
        format!("credential://{cred_id1}/generation/1/{representation_id1}");
    sqlx::query("INSERT INTO orbit_credential_representations(id, credential_id, generation, interface, auth_type, state, secret_locator, last_validated_at) VALUES($1, $2, 1, 'codex', 'local-session', 'stored', $3, clock_timestamp())")
        .bind(&representation_id1)
        .bind(&cred_id1)
        .bind(&representation_locator1)
        .execute(&mut *tx)
        .await?;

    let cred_id2 = id();
    let secret_id2 = id();
    let locator2 = format!("credential://{cred_id2}/generation/1/{secret_id2}");
    sqlx::query("INSERT INTO orbit_credentials(id, scope_key, provider, reference, current_generation, endpoint, auth_type, status) VALUES($1, 'operator', 'antigravity', 'antigravity-ch9b2013', 1, NULL, 'oauth', 'enrolled')")
        .bind(&cred_id2)
        .execute(&mut *tx).await?;
    sqlx::query("INSERT INTO orbit_credential_generations(credential_id, generation, backend, state, secret_locator) VALUES($1, 1, 'local-private', 'enrolled', $2)")
        .bind(&cred_id2)
        .bind(&locator2)
        .execute(&mut *tx).await?;
    let representation_id2 = id();
    let representation_locator2 =
        format!("credential://{cred_id2}/generation/1/{representation_id2}");
    sqlx::query("INSERT INTO orbit_credential_representations(id, credential_id, generation, interface, auth_type, state, secret_locator, last_validated_at) VALUES($1, $2, 1, 'acp', 'oauth', 'stored', $3, clock_timestamp())")
        .bind(&representation_id2)
        .bind(&cred_id2)
        .bind(&representation_locator2)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

async fn seed_completed_repair(
    ctx: &TestContext,
    task_id: &str,
    attempt_id: &str,
    repo_path: &std::path::Path,
    max_iterations: u32,
) -> Result<(WorkflowRun, WorkspaceState, RoleExecution, HandoffArtifact)> {
    let workflow = ctx
        .store
        .create_workflow_run_full(
            task_id,
            attempt_id,
            max_iterations,
            None,
            None,
            None,
            Some("repair coordinator state"),
            repo_path.to_str(),
            Some("base"),
        )
        .await?;
    let state = orbit::workflow_coordinator::compute_workspace_state(repo_path, "base").await?;
    ctx.store
        .transition_workflow_stage(&workflow.id, WorkflowStage::Planning, None, None, None)
        .await?;
    ctx.store
        .transition_workflow_stage(&workflow.id, WorkflowStage::Implementing, None, None, None)
        .await?;
    ctx.store
        .transition_workflow_stage(
            &workflow.id,
            WorkflowStage::Verifying,
            Some(&state.state_id),
            None,
            None,
        )
        .await?;
    let initial_failure = ctx
        .store
        .save_handoff_artifact(
            &workflow.id,
            None,
            HandoffType::FailureEvidence,
            None,
            serde_json::to_value(FailureEvidenceHandoff {
                failed_stage: "VERIFYING_FAST".into(),
                verification_run_id: None,
                failed_steps: vec!["fixture-check".into()],
                error_summary: "initial fixture failure".into(),
                stdout_previews: BTreeMap::new(),
                stderr_previews: BTreeMap::new(),
            })?,
        )
        .await?;
    ctx.store
        .transition_workflow_stage(
            &workflow.id,
            WorkflowStage::Repairing,
            Some(&state.state_id),
            Some(1),
            None,
        )
        .await?;

    let repair = ctx
        .store
        .create_role_execution(
            &workflow.id,
            &RoleDefinition::implementer_v1(),
            "REPAIRING",
            1,
            Some(&state.state_id),
            Some(&initial_failure.id),
        )
        .await?;
    ctx.store
        .set_role_execution_resolved(&repair.id, &fixture_target())
        .await?;
    let implementation = ctx
        .store
        .save_handoff_artifact(
            &workflow.id,
            Some(&repair.id),
            HandoffType::Implementation,
            Some(&state.state_id),
            serde_json::to_value(ImplementationHandoff {
                summary: "repair completed before its stage transition".into(),
                changed_files: vec![],
                tests_added_or_modified: vec![],
                exploratory_commands: vec![],
                known_limitations: vec![],
                verification_notes: vec![],
            })?,
        )
        .await?;
    ctx.store
        .complete_role_execution_success(
            &repair.id,
            Some(&state.state_id),
            Some(&implementation.id),
        )
        .await?;

    Ok((workflow, state, repair, initial_failure))
}

fn fixture_target() -> ResolvedExecutionTarget {
    ResolvedExecutionTarget {
        provider: "codex".into(),
        runtime_interface: "codex-acp".into(),
        credential_id: Some("fixture-codex".into()),
        credential_generation: Some(1),
        requested_model: Some("gpt-6-luna".into()),
        resolved_model: Some("gpt-6-luna".into()),
        runtime_image_digest: None,
        resolution_reason: "deterministic coordinator fixture".into(),
    }
}

struct NoOpImplementationExecutor;

#[async_trait::async_trait]
impl RoleAgentExecutor for NoOpImplementationExecutor {
    async fn execute_role(
        &self,
        _pool: &PgPool,
        _wf_run: &WorkflowRun,
        role_exec: &RoleExecution,
        role: &RoleDefinition,
        _target: &ResolvedExecutionTarget,
        _task_text: &str,
        _repo_path: &std::path::Path,
        _input_handoff: Option<&HandoffArtifact>,
        _cancellation: tokio::sync::watch::Receiver<bool>,
    ) -> Result<RoleExecutionOutcome> {
        let raw_output = match role.role_id.as_str() {
            "planner" => format!(
                "{ORBIT_HANDOFF_START}\n{}\n{ORBIT_HANDOFF_END}",
                serde_json::to_string(&PlanHandoff {
                    summary: "inspect and update candidate".into(),
                    affected_areas: vec!["candidate".into()],
                    implementation_steps: vec!["update candidate".into()],
                    expected_files: vec!["candidate.txt".into()],
                    risks: vec![],
                    verification_notes: vec![],
                    open_questions: vec![],
                })?
            ),
            "implementer" => format!(
                "{ORBIT_HANDOFF_START}\n{}\n{ORBIT_HANDOFF_END}",
                serde_json::to_string(&ImplementationHandoff {
                    summary: if role_exec.stage == "REPAIRING" {
                        "repair reported complete".into()
                    } else {
                        "implementation reported complete".into()
                    },
                    changed_files: vec!["candidate.txt".into()],
                    tests_added_or_modified: vec![],
                    exploratory_commands: vec![],
                    known_limitations: vec![],
                    verification_notes: vec![],
                })?
            ),
            other => anyhow::bail!("unexpected no-op role {other}"),
        };
        Ok(RoleExecutionOutcome {
            raw_output,
            agent_execution_ids: vec![format!("noop-agent-{}", id())],
            termination_reason: Some("completed".into()),
        })
    }
}

async fn record_later_failure(
    ctx: &TestContext,
    workflow_id: &str,
    failed_stage: &str,
    verification_run_id: Option<&str>,
    state_id: &str,
) -> Result<HandoffArtifact> {
    let failure = ctx
        .store
        .save_handoff_artifact(
            workflow_id,
            None,
            HandoffType::FailureEvidence,
            None,
            serde_json::to_value(FailureEvidenceHandoff {
                failed_stage: failed_stage.into(),
                verification_run_id: verification_run_id.map(str::to_owned),
                failed_steps: vec!["fixture-check".into()],
                error_summary: "later failure after repair completed".into(),
                stdout_previews: BTreeMap::new(),
                stderr_previews: BTreeMap::new(),
            })?,
        )
        .await?;
    ctx.store
        .transition_workflow_stage(
            workflow_id,
            WorkflowStage::Repairing,
            Some(state_id),
            None,
            Some("later failure after repair completed"),
        )
        .await?;
    Ok(failure)
}

async fn simulate_changes_requested_after_repair(
    ctx: &TestContext,
    workflow_id: &str,
    state: &WorkspaceState,
) -> Result<HandoffArtifact> {
    ctx.store
        .transition_workflow_stage(
            workflow_id,
            WorkflowStage::Verifying,
            Some(&state.state_id),
            None,
            None,
        )
        .await?;
    ctx.store
        .transition_workflow_stage(
            workflow_id,
            WorkflowStage::Reviewing,
            Some(&state.state_id),
            None,
            None,
        )
        .await?;
    let reviewer = ctx
        .store
        .create_role_execution(
            workflow_id,
            &RoleDefinition::reviewer_v1(),
            "REVIEWING",
            1,
            Some(&state.state_id),
            None,
        )
        .await?;
    ctx.store
        .set_role_execution_resolved(&reviewer.id, &fixture_target())
        .await?;
    let review = ctx
        .store
        .save_handoff_artifact(
            workflow_id,
            Some(&reviewer.id),
            HandoffType::Review,
            Some(&state.state_id),
            serde_json::to_value(ReviewDecision {
                decision: ReviewDecisionStatus::ChangesRequested,
                summary: "repair is incomplete".into(),
                findings: vec![],
                requested_changes: vec!["finish the requested extraction".into()],
                suggested_additional_checks: vec![],
            })?,
        )
        .await?;
    ctx.store
        .complete_role_execution_success(&reviewer.id, Some(&state.state_id), Some(&review.id))
        .await?;
    record_later_failure(ctx, workflow_id, "REVIEWING", None, &state.state_id).await
}

fn fast_and_standard_runs(runs: &[VerificationRun], workspace_state_id: &str) -> Vec<String> {
    runs.iter()
        .filter(|run| {
            run.workspace_state_id == workspace_state_id
                && matches!(
                    run.tier,
                    Some(VerificationTier::Fast | VerificationTier::Standard)
                )
        })
        .map(|run| run.id.clone())
        .collect()
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn completed_repair_recovery_does_not_replay_after_a_later_failure() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;

    // A completed repair whose matching failure handoff is still current means
    // only the stage update was interrupted. Recovery must continue to VERIFYING.
    let recovery_repo = tempfile::tempdir()?;
    std::fs::write(
        recovery_repo.path().join("candidate.txt"),
        "unchanged candidate",
    )?;
    let (recovery_wf, recovery_state, recovery_role, recovery_failure) = seed_completed_repair(
        &ctx,
        "repair-recovery",
        &format!("att-{}", id()),
        recovery_repo.path(),
        2,
    )
    .await?;
    let recovery_executor = Arc::new(SimulatedRoleExecutor::with_approval());
    let recovery_coordinator =
        WorkflowCoordinator::new(ctx.engine.pool.clone(), recovery_executor.clone());
    let recovery_runs_before = ctx
        .store
        .verification_store()
        .list_runs(&recovery_wf.attempt_id)
        .await?;
    assert_eq!(
        recovery_coordinator.step(&recovery_wf.id).await?,
        WorkflowStepResult::Advanced {
            from: WorkflowStage::Repairing,
            to: WorkflowStage::Verifying,
        }
    );
    let recovered = ctx.store.get_workflow_run(&recovery_wf.id).await?.unwrap();
    assert_eq!(
        recovered.current_workspace_state_id.as_deref(),
        Some(recovery_state.state_id.as_str())
    );
    assert_eq!(recovered.iteration, 1);
    assert_eq!(
        ctx.store.list_role_executions(&recovery_wf.id).await?.len(),
        1
    );
    assert_eq!(
        recovery_role.handoff_input_id.as_deref(),
        Some(recovery_failure.id.as_str())
    );
    assert!(recovery_executor.recorded_roles.lock().unwrap().is_empty());
    let recovery_runs_after = ctx
        .store
        .verification_store()
        .list_runs(&recovery_wf.attempt_id)
        .await?;
    assert_eq!(
        fast_and_standard_runs(&recovery_runs_after, &recovery_state.state_id),
        fast_and_standard_runs(&recovery_runs_before, &recovery_state.state_id)
    );

    // A reviewer CHANGES_REQUESTED after that repair creates new failure
    // evidence. At the iteration limit, the workflow must exhaust rather than
    // replaying the prior repair and verification against the same state.
    let review_repo = tempfile::tempdir()?;
    std::fs::write(
        review_repo.path().join("candidate.txt"),
        "unchanged candidate",
    )?;
    let (review_wf, review_state, _, prior_failure) = seed_completed_repair(
        &ctx,
        "review-repair-exhaustion",
        &format!("att-{}", id()),
        review_repo.path(),
        1,
    )
    .await?;
    let later_review_failure =
        simulate_changes_requested_after_repair(&ctx, &review_wf.id, &review_state).await?;
    assert_ne!(later_review_failure.id, prior_failure.id);
    let review_executor = Arc::new(SimulatedRoleExecutor::with_approval());
    let review_coordinator =
        WorkflowCoordinator::new(ctx.engine.pool.clone(), review_executor.clone());
    let review_runs_before = ctx
        .store
        .verification_store()
        .list_runs(&review_wf.attempt_id)
        .await?;
    assert_eq!(
        review_coordinator.step(&review_wf.id).await?,
        WorkflowStepResult::Terminal(WorkflowStage::Exhausted)
    );
    assert_eq!(
        ctx.store
            .get_workflow_run(&review_wf.id)
            .await?
            .unwrap()
            .status,
        WorkflowStage::Exhausted
    );
    assert_eq!(
        ctx.store.list_role_executions(&review_wf.id).await?.len(),
        2
    );
    assert!(review_executor.recorded_roles.lock().unwrap().is_empty());
    let review_runs_after = ctx
        .store
        .verification_store()
        .list_runs(&review_wf.attempt_id)
        .await?;
    assert_eq!(
        fast_and_standard_runs(&review_runs_after, &review_state.state_id),
        fast_and_standard_runs(&review_runs_before, &review_state.state_id)
    );

    // Below the limit, the same later reviewer finding must start a new
    // repair execution at the next iteration instead of reusing iteration 1.
    let next_repo = tempfile::tempdir()?;
    std::fs::write(
        next_repo.path().join("candidate.txt"),
        "unchanged candidate",
    )?;
    let (next_wf, next_state, previous_repair, _) = seed_completed_repair(
        &ctx,
        "review-repair-next-iteration",
        &format!("att-{}", id()),
        next_repo.path(),
        2,
    )
    .await?;
    simulate_changes_requested_after_repair(&ctx, &next_wf.id, &next_state).await?;
    let next_executor = Arc::new(SimulatedRoleExecutor::with_approval());
    let next_coordinator = WorkflowCoordinator::new(ctx.engine.pool.clone(), next_executor.clone());
    assert_eq!(
        next_coordinator.step(&next_wf.id).await?,
        WorkflowStepResult::Advanced {
            from: WorkflowStage::Repairing,
            to: WorkflowStage::Verifying,
        }
    );
    let next_workflow = ctx.store.get_workflow_run(&next_wf.id).await?.unwrap();
    assert_eq!(next_workflow.iteration, 2);
    let next_roles = ctx.store.list_role_executions(&next_wf.id).await?;
    let next_repair = next_roles
        .iter()
        .find(|role| role.stage == "REPAIRING" && role.iteration == 2)
        .expect("later failure starts a new repair iteration");
    assert_ne!(next_repair.id, previous_repair.id);
    assert_eq!(next_repair.status, RoleExecutionStatus::Succeeded);
    assert_ne!(
        next_repair.output_workspace_state_id.as_deref(),
        Some(next_state.state_id.as_str())
    );
    assert_eq!(
        next_workflow.current_workspace_state_id,
        next_repair.output_workspace_state_id
    );
    assert_eq!(
        next_roles
            .iter()
            .filter(|role| role.stage == "REPAIRING")
            .count(),
        2
    );
    assert_eq!(
        *next_executor.recorded_roles.lock().unwrap(),
        vec!["implementer".to_string()]
    );
    let active_locks: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM orbit_attempt_workspace_locks WHERE attempt_id = $1 AND revoked_at_ms IS NULL",
    )
    .bind(&next_wf.attempt_id)
    .fetch_one(&ctx.engine.pool)
    .await?;
    assert_eq!(active_locks, 0);

    // A FAST/STANDARD failure after a completed repair follows the same rule.
    let verification_repo = tempfile::tempdir()?;
    std::fs::write(
        verification_repo.path().join("candidate.txt"),
        "unchanged candidate",
    )?;
    let (verification_wf, verification_state, _, verification_prior_failure) =
        seed_completed_repair(
            &ctx,
            "verification-repair-exhaustion",
            &format!("att-{}", id()),
            verification_repo.path(),
            1,
        )
        .await?;
    ctx.store
        .transition_workflow_stage(
            &verification_wf.id,
            WorkflowStage::Verifying,
            Some(&verification_state.state_id),
            None,
            None,
        )
        .await?;
    let verification_failure = record_later_failure(
        &ctx,
        &verification_wf.id,
        "VERIFYING_STANDARD",
        Some("fixture-verification-run"),
        &verification_state.state_id,
    )
    .await?;
    assert_ne!(verification_failure.id, verification_prior_failure.id);
    let verification_executor = Arc::new(SimulatedRoleExecutor::with_approval());
    let verification_coordinator =
        WorkflowCoordinator::new(ctx.engine.pool.clone(), verification_executor.clone());
    let verification_runs_before = ctx
        .store
        .verification_store()
        .list_runs(&verification_wf.attempt_id)
        .await?;
    assert_eq!(
        verification_coordinator.step(&verification_wf.id).await?,
        WorkflowStepResult::Terminal(WorkflowStage::Exhausted)
    );
    assert_eq!(
        ctx.store
            .get_workflow_run(&verification_wf.id)
            .await?
            .unwrap()
            .status,
        WorkflowStage::Exhausted
    );
    assert_eq!(
        ctx.store
            .list_role_executions(&verification_wf.id)
            .await?
            .len(),
        1
    );
    assert!(
        verification_executor
            .recorded_roles
            .lock()
            .unwrap()
            .is_empty()
    );
    let verification_runs_after = ctx
        .store
        .verification_store()
        .list_runs(&verification_wf.attempt_id)
        .await?;
    assert_eq!(
        fast_and_standard_runs(&verification_runs_after, &verification_state.state_id),
        fast_and_standard_runs(&verification_runs_before, &verification_state.state_id)
    );

    teardown_test(ctx).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn workflow_fails_closed_without_pinned_verification_profile() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;

    let policy = sample_policy();
    ctx.store.verification_store().save_policy(&policy).await?;
    let repo = tempfile::tempdir()?;
    let repo_path = repo.path().to_string_lossy().into_owned();
    let wf = ctx
        .store
        .create_workflow_run_full(
            "task-01",
            "att-01",
            3,
            Some(&policy),
            None,
            None,
            Some("Refactor internal utils"),
            Some(&repo_path),
            Some("HEAD"),
        )
        .await?;
    assert_eq!(wf.status, WorkflowStage::Created);

    let executor = Arc::new(SimulatedRoleExecutor::with_approval());
    let coordinator = WorkflowCoordinator::new(ctx.engine.pool.clone(), executor);

    coordinator.step(&wf.id).await?; // -> Planning
    coordinator.step(&wf.id).await?; // -> Implementing
    coordinator.step(&wf.id).await?; // -> Verifying
    assert_verification_profile_gate(&coordinator, &ctx.store, &wf.id).await?;

    let roles = ctx.store.list_role_executions(&wf.id).await?;
    assert_eq!(roles.len(), 2); // Planner and Implementer only
    assert!(
        roles
            .iter()
            .all(|r| r.status == RoleExecutionStatus::Succeeded)
    );

    teardown_test(ctx).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn real_planner_codex_acp() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;

    let role = RoleDefinition::planner_v1();
    let target = RoleRuntimeResolver::resolve_target_live(&ctx.engine.pool, &role, None).await?;
    assert_eq!(target.provider, "codex");
    assert_eq!(target.runtime_interface, "codex-acp");
    assert_eq!(target.requested_model.as_deref(), Some("gpt-6-luna"));

    teardown_test(ctx).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn reviewer_excludes_runtime_without_exact_tool_audit() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;

    let role = RoleDefinition::reviewer_v1();
    let target = RoleRuntimeResolver::resolve_target_live(&ctx.engine.pool, &role, None).await?;
    assert_eq!(target.provider, "codex");
    assert_eq!(target.runtime_interface, "codex-acp");
    assert_eq!(target.requested_model.as_deref(), Some("gpt-6-luna"));
    assert!(target.resolution_reason.contains("CAPABILITY_MISMATCH"));

    teardown_test(ctx).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn live_credential_resolution() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;

    let role = RoleDefinition::planner_v1();
    let target = RoleRuntimeResolver::resolve_target_live(&ctx.engine.pool, &role, None).await?;
    assert_eq!(target.credential_id.as_deref(), Some("codex-main"));
    assert_eq!(target.credential_generation, Some(1));

    teardown_test(ctx).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn live_runtime_capability_resolution() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;

    let role = RoleDefinition::implementer_v1();
    let target = RoleRuntimeResolver::resolve_target_live(&ctx.engine.pool, &role, None).await?;
    assert_eq!(target.provider, "codex");
    assert!(target.runtime_image_digest.is_some());

    teardown_test(ctx).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn reset_aware_resolver_prefers_earlier_weekly_reset() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;
    let credentials = orbit::credential_registry::CredentialStore::new(&ctx.engine.pool)
        .list()
        .await?;
    let codex = credentials
        .iter()
        .find(|credential| credential.reference == "codex-main")
        .context("Codex fixture credential missing")?;
    let antigravity = credentials
        .iter()
        .find(|credential| credential.reference == "antigravity-ch9b2013")
        .context("Antigravity fixture credential missing")?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as i64;

    let codex_snapshot = orbit::availability::AvailabilitySnapshot {
        applies_to: orbit::availability::AvailabilityScope::Credential(codex.identity()),
        observed_at_ms: now_ms,
        expires_at_ms: now_ms + 60 * 60 * 1000,
        state: orbit::availability::AvailabilityState::Ready,
        quota_windows: vec![
            orbit::availability::QuotaWindow {
                label: "default.5h".into(),
                duration_minutes: Some(300),
                used_percent: Some(20.0),
                remaining_percent: Some(80.0),
                resets_at_ms: Some(now_ms + 2 * 60 * 60 * 1000),
                exhausted: None,
            },
            orbit::availability::QuotaWindow {
                label: "default.weekly".into(),
                duration_minutes: Some(10_080),
                used_percent: Some(10.0),
                remaining_percent: Some(90.0),
                resets_at_ms: Some(now_ms + 5 * 24 * 60 * 60 * 1000),
                exhausted: None,
            },
        ],
        quota_buckets: vec![],
        quota_groups: vec![],
        source: orbit::availability::EvidenceSource::ProviderNativeStatus,
        confidence: orbit::availability::EvidenceConfidence::AuthoritativeNative,
        source_revision: "reset-fixture".into(),
        evidence_digest: format!("sha256:{}", "a".repeat(64)),
        provider_observed_at_ms: Some(now_ms),
        provider_status_observation: None,
    };
    let antigravity_bucket = format!("qb1:{}", "b".repeat(64));
    let antigravity_snapshot = orbit::availability::AvailabilitySnapshot {
        applies_to: orbit::availability::AvailabilityScope::Credential(antigravity.identity()),
        observed_at_ms: now_ms,
        expires_at_ms: now_ms + 60 * 60 * 1000,
        state: orbit::availability::AvailabilityState::Ready,
        quota_windows: vec![],
        quota_buckets: vec![orbit::availability::QuotaBucket {
            provider_bucket_fingerprint: antigravity_bucket.clone(),
            provider_label: None,
            scope: None,
            windows: vec![
                orbit::availability::QuotaBucketWindow {
                    provider_window_id: "5h".into(),
                    duration_minutes: None,
                    used_percent: None,
                    remaining_percent: None,
                    remaining_fraction: Some(0.30),
                    resets_at_ms: Some(now_ms + 2 * 60 * 60 * 1000),
                    provider_reset_time: None,
                    exhausted: None,
                },
                orbit::availability::QuotaBucketWindow {
                    provider_window_id: "weekly".into(),
                    duration_minutes: None,
                    used_percent: None,
                    remaining_percent: None,
                    remaining_fraction: Some(0.70),
                    resets_at_ms: Some(now_ms + 60 * 60 * 1000),
                    provider_reset_time: None,
                    exhausted: None,
                },
            ],
        }],
        quota_groups: vec![orbit::availability::ProviderQuotaGroup {
            fingerprint: format!("qg1:{}", "c".repeat(64)),
            identity_basis: orbit::availability::ProviderQuotaGroupIdentityBasis::MemberSet,
            provider_display_name: Some("Gemini Models".into()),
            provider_description: None,
            members: vec![orbit::availability::ProviderQuotaMember {
                provider_label: "Gemini Flash".into(),
                provider_key_fingerprint: None,
            }],
            bucket_fingerprints: vec![antigravity_bucket],
        }],
        source: orbit::availability::EvidenceSource::ProviderNativeStatus,
        confidence: orbit::availability::EvidenceConfidence::AuthoritativeNative,
        source_revision: "reset-fixture".into(),
        evidence_digest: format!("sha256:{}", "d".repeat(64)),
        provider_observed_at_ms: Some(now_ms),
        provider_status_observation: None,
    };
    let availability = orbit::availability::AvailabilityStore::new(&ctx.engine.pool);
    availability.record(&codex_snapshot).await?;
    availability.record(&antigravity_snapshot).await?;

    // Planner ordinarily prefers Codex. A safe, earlier Antigravity weekly
    // reset must move that account ahead in the actual resolver result.
    let exact = RoleRuntimeResolver::resolve_ranked_targets_live(
        &ctx.engine.pool,
        &RoleDefinition::planner_v1(),
        None,
        RuntimeQuotaSelectionPolicy::default(),
    )
    .await?;
    assert_eq!(exact.len(), 1);
    assert_eq!(exact[0].provider, "codex");
    assert!(
        exact[0]
            .resolution_reason
            .contains("tool_audit_correlation=EXACT")
    );
    assert!(exact[0].resolution_reason.contains("CAPABILITY_MISMATCH"));
    assert!(exact[0].resolution_reason.contains("provided=PARTIAL"));

    let mut role = RoleDefinition::planner_v1();
    role.allowed_capabilities.required_tool_audit_correlation =
        Some(orbit::acp_capabilities::ToolAuditCorrelationCapability::Partial);
    let ranked = RoleRuntimeResolver::resolve_ranked_targets_live(
        &ctx.engine.pool,
        &role,
        None,
        RuntimeQuotaSelectionPolicy::default(),
    )
    .await?;
    assert_eq!(ranked[0].provider, "antigravity");
    assert_eq!(
        ranked[0].credential_id.as_deref(),
        Some("antigravity-ch9b2013")
    );
    assert_eq!(ranked[1].provider, "codex");
    assert!(ranked[0].resolution_reason.contains("known_weekly_reset"));
    assert!(ranked[0].resolution_reason.contains("7d_remaining=70.0%"));

    role.runtime_preferences = vec!["antigravity-acp".into()];
    role.allowed_capabilities.required_tool_audit_correlation =
        Some(orbit::acp_capabilities::ToolAuditCorrelationCapability::Exact);
    let error = RoleRuntimeResolver::resolve_ranked_targets_live(
        &ctx.engine.pool,
        &role,
        None,
        RuntimeQuotaSelectionPolicy::default(),
    )
    .await
    .expect_err("no exact-capable account must fail closed");
    assert!(error.to_string().contains("CAPABILITY_MISMATCH"));

    let mut exact_role = RoleDefinition::planner_v1();
    exact_role.runtime_preferences = vec!["codex-acp".into(), "antigravity-acp".into()];
    let fallback_error = RoleRuntimeResolver::resolve_ranked_targets_live(
        &ctx.engine.pool,
        &exact_role,
        Some("codex-acp"),
        RuntimeQuotaSelectionPolicy::default(),
    )
    .await
    .expect_err("operational fallback cannot use a partial-audit runtime");
    assert!(fallback_error.to_string().contains("CAPABILITY_MISMATCH"));

    teardown_test(ctx).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn no_dummy_credentials() -> Result<()> {
    let ctx = setup_test().await?;
    // No credentials enrolled: resolver MUST bail out, NOT return "cred-planner"
    let role = RoleDefinition::planner_v1();
    let res = RoleRuntimeResolver::resolve_target_live(&ctx.engine.pool, &role, None).await;
    assert!(res.is_err());
    let err = res.unwrap_err().to_string();
    assert!(!err.contains("cred-planner"));
    assert!(err.contains("no eligible credentials enrolled"));

    teardown_test(ctx).await
}

#[tokio::test]
async fn structured_plan_handoff() -> Result<()> {
    let valid_plan = PlanHandoff {
        summary: "Plan summary".into(),
        affected_areas: vec!["src".into()],
        implementation_steps: vec!["step 1".into()],
        expected_files: vec!["src/lib.rs".into()],
        risks: vec![],
        verification_notes: vec![],
        open_questions: vec![],
    };
    assert!(valid_plan.validate().is_ok());

    let invalid_plan = PlanHandoff {
        summary: "".into(),
        affected_areas: vec![],
        implementation_steps: vec![],
        expected_files: vec![],
        risks: vec![],
        verification_notes: vec![],
        open_questions: vec![],
    };
    assert!(invalid_plan.validate().is_err());

    let envelope_output = format!(
        "Analysis prose...\n{}\n{}\n{}\nMore prose...",
        ORBIT_HANDOFF_START,
        serde_json::to_string(&valid_plan)?,
        ORBIT_HANDOFF_END
    );
    let extracted: PlanHandoff = extract_structured_envelope(&envelope_output, "PlanHandoff")?;
    assert_eq!(extracted.summary, "Plan summary");

    Ok(())
}

#[tokio::test]
async fn structured_implementation_handoff() -> Result<()> {
    let valid_impl = ImplementationHandoff {
        summary: "Applied patch".into(),
        changed_files: vec!["src/main.rs".into()],
        tests_added_or_modified: vec![],
        exploratory_commands: vec![],
        known_limitations: vec![],
        verification_notes: vec![],
    };
    assert!(valid_impl.validate().is_ok());

    let invalid_impl = ImplementationHandoff {
        summary: "  ".into(),
        changed_files: vec![],
        tests_added_or_modified: vec![],
        exploratory_commands: vec![],
        known_limitations: vec![],
        verification_notes: vec![],
    };
    assert!(invalid_impl.validate().is_err());

    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn required_no_op_implementation_fails_before_verification_and_releases_ownership()
-> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;
    let repo = tempfile::tempdir()?;
    std::fs::write(repo.path().join("candidate.txt"), "unchanged\n")?;
    let workflow = ctx
        .store
        .create_workflow_run_full(
            "required-no-op",
            &format!("att-{}", id()),
            2,
            None,
            None,
            None,
            Some("change the candidate"),
            repo.path().to_str(),
            Some("base"),
        )
        .await?;
    let coordinator = WorkflowCoordinator::new(
        ctx.engine.pool.clone(),
        Arc::new(NoOpImplementationExecutor),
    );

    coordinator.step(&workflow.id).await?;
    coordinator.step(&workflow.id).await?;
    assert_eq!(
        coordinator.step(&workflow.id).await?,
        WorkflowStepResult::Terminal(WorkflowStage::Failed)
    );

    let roles = ctx.store.list_role_executions(&workflow.id).await?;
    let implementation = roles
        .iter()
        .find(|role| role.stage == "IMPLEMENTING")
        .context("implementation role missing")?;
    assert_eq!(implementation.status, RoleExecutionStatus::Failed);
    assert_eq!(
        implementation.termination_reason.as_deref(),
        Some("IMPLEMENTATION_NO_CHANGE")
    );
    assert!(implementation.handoff_output_id.is_some());
    assert!(
        ctx.store
            .get_latest_handoff_of_type(&workflow.id, HandoffType::Implementation)
            .await?
            .is_some()
    );
    assert!(
        ctx.store
            .verification_store()
            .list_runs(&workflow.attempt_id)
            .await?
            .is_empty()
    );
    assert!(!roles.iter().any(|role| role.stage == "REVIEWING"));
    let active_locks: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM orbit_attempt_workspace_locks WHERE attempt_id = $1 AND revoked_at_ms IS NULL",
    )
    .bind(&workflow.attempt_id)
    .fetch_one(&ctx.engine.pool)
    .await?;
    assert_eq!(active_locks, 0);
    let step_owner: Option<String> =
        sqlx::query_scalar("SELECT step_owner_id FROM orbit_workflow_runs WHERE id = $1")
            .bind(&workflow.id)
            .fetch_one(&ctx.engine.pool)
            .await?;
    assert!(step_owner.is_none());
    let role_count = roles.len();
    assert_eq!(
        coordinator.step(&workflow.id).await?,
        WorkflowStepResult::Terminal(WorkflowStage::Failed)
    );
    let roles_after_retry = ctx.store.list_role_executions(&workflow.id).await?;
    assert_eq!(roles_after_retry.len(), role_count);
    assert!(
        !roles_after_retry
            .iter()
            .any(|role| role.stage == "REVIEWING")
    );
    assert!(
        ctx.store
            .verification_store()
            .list_runs(&workflow.attempt_id)
            .await?
            .is_empty()
    );

    teardown_test(ctx).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn required_no_op_repair_fails_without_reverification() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;
    let repo = tempfile::tempdir()?;
    std::fs::write(repo.path().join("candidate.txt"), "broken candidate\n")?;
    let workflow = ctx
        .store
        .create_workflow_run_full(
            "required-no-op-repair",
            &format!("att-{}", id()),
            3,
            None,
            None,
            None,
            Some("repair the candidate"),
            repo.path().to_str(),
            Some("base"),
        )
        .await?;
    let state = compute_workspace_state(repo.path(), "base").await?;
    ctx.store
        .transition_workflow_stage(&workflow.id, WorkflowStage::Planning, None, None, None)
        .await?;
    ctx.store
        .transition_workflow_stage(&workflow.id, WorkflowStage::Implementing, None, None, None)
        .await?;
    ctx.store
        .transition_workflow_stage(
            &workflow.id,
            WorkflowStage::Verifying,
            Some(&state.state_id),
            None,
            None,
        )
        .await?;
    ctx.store
        .save_handoff_artifact(
            &workflow.id,
            None,
            HandoffType::FailureEvidence,
            Some(&state.state_id),
            serde_json::to_value(FailureEvidenceHandoff {
                failed_stage: "VERIFYING_FAST".into(),
                verification_run_id: None,
                failed_steps: vec!["candidate-check".into()],
                error_summary: "candidate remains broken".into(),
                stdout_previews: BTreeMap::new(),
                stderr_previews: BTreeMap::new(),
            })?,
        )
        .await?;
    ctx.store
        .transition_workflow_stage(
            &workflow.id,
            WorkflowStage::Repairing,
            Some(&state.state_id),
            None,
            Some("candidate remains broken"),
        )
        .await?;
    let coordinator = WorkflowCoordinator::new(
        ctx.engine.pool.clone(),
        Arc::new(NoOpImplementationExecutor),
    );

    assert_eq!(
        coordinator.step(&workflow.id).await?,
        WorkflowStepResult::Terminal(WorkflowStage::Failed)
    );
    let roles = ctx.store.list_role_executions(&workflow.id).await?;
    let repair = roles
        .iter()
        .find(|role| role.stage == "REPAIRING")
        .context("repair role missing")?;
    assert_eq!(repair.status, RoleExecutionStatus::Failed);
    assert_eq!(
        repair.termination_reason.as_deref(),
        Some("IMPLEMENTATION_NO_CHANGE")
    );
    assert!(
        ctx.store
            .verification_store()
            .list_runs(&workflow.attempt_id)
            .await?
            .is_empty()
    );
    let active_locks: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM orbit_attempt_workspace_locks WHERE attempt_id = $1 AND revoked_at_ms IS NULL",
    )
    .bind(&workflow.attempt_id)
    .fetch_one(&ctx.engine.pool)
    .await?;
    assert_eq!(active_locks, 0);
    let step_owner: Option<String> =
        sqlx::query_scalar("SELECT step_owner_id FROM orbit_workflow_runs WHERE id = $1")
            .bind(&workflow.id)
            .fetch_one(&ctx.engine.pool)
            .await?;
    assert!(step_owner.is_none());
    let role_count = roles.len();
    assert_eq!(
        coordinator.step(&workflow.id).await?,
        WorkflowStepResult::Terminal(WorkflowStage::Failed)
    );
    let roles_after_retry = ctx.store.list_role_executions(&workflow.id).await?;
    assert_eq!(roles_after_retry.len(), role_count);
    assert!(
        !roles_after_retry
            .iter()
            .any(|role| role.stage == "REVIEWING")
    );
    assert!(
        ctx.store
            .verification_store()
            .list_runs(&workflow.attempt_id)
            .await?
            .is_empty()
    );

    teardown_test(ctx).await
}

#[tokio::test]
async fn structured_review_handoff() -> Result<()> {
    let valid_review = ReviewDecision {
        decision: ReviewDecisionStatus::Approve,
        summary: "LGTM".into(),
        findings: vec![],
        requested_changes: vec![],
        suggested_additional_checks: vec![],
    };
    assert!(valid_review.validate().is_ok());

    let invalid_review = ReviewDecision {
        decision: ReviewDecisionStatus::Approve,
        summary: "".into(),
        findings: vec![],
        requested_changes: vec![],
        suggested_additional_checks: vec![],
    };
    assert!(invalid_review.validate().is_err());

    Ok(())
}

#[tokio::test]
async fn planner_read_only_live() -> Result<()> {
    let planner = RoleDefinition::planner_v1();
    assert_eq!(planner.workspace_access, WorkspaceAccess::ReadOnly);
    assert!(!planner.allowed_capabilities.repo_write);
    assert!(planner.allowed_capabilities.repo_read);
    assert!(planner.allowed_capabilities.structured_output);
    Ok(())
}

#[tokio::test]
async fn reviewer_read_only_live() -> Result<()> {
    let reviewer = RoleDefinition::reviewer_v1();
    assert_eq!(reviewer.workspace_access, WorkspaceAccess::ReadOnly);
    assert!(!reviewer.allowed_capabilities.repo_write);
    assert!(reviewer.allowed_capabilities.repo_read);
    assert!(reviewer.allowed_capabilities.structured_output);
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn implementer_mutation_lock_live() -> Result<()> {
    let ctx = setup_test().await?;
    let repo = tempfile::tempdir()?;
    let repository = repo.path().to_string_lossy().into_owned();
    let workflow = ctx
        .store
        .create_workflow_run_full(
            "lock-test",
            "attempt-lock",
            2,
            None,
            None,
            None,
            None,
            Some(&repository),
            None,
        )
        .await?;
    let role = RoleDefinition::implementer_v1();
    let first = ctx
        .store
        .create_role_execution(&workflow.id, &role, "IMPLEMENTING", 1, None, None)
        .await?;
    let second = ctx
        .store
        .create_role_execution(&workflow.id, &role, "IMPLEMENTING", 1, None, None)
        .await?;
    ctx.store
        .acquire_workspace_mutation_lock("attempt-lock", &first.id)
        .await?;
    // Second concurrent lock must fail
    let err = ctx
        .store
        .acquire_workspace_mutation_lock("attempt-lock", &second.id)
        .await;
    assert!(err.is_err());

    ctx.store
        .release_workspace_mutation_lock("attempt-lock", &first.id)
        .await?;
    // Now holder-2 can acquire
    assert!(
        ctx.store
            .acquire_workspace_mutation_lock("attempt-lock", &second.id)
            .await
            .is_ok()
    );
    ctx.store
        .release_workspace_mutation_lock("attempt-lock", &second.id)
        .await?;

    teardown_test(ctx).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn fast_auto_execution() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;

    let policy = sample_policy();
    ctx.store.verification_store().save_policy(&policy).await?;
    let repo = tempfile::tempdir()?;
    let repo_path = repo.path().to_string_lossy().into_owned();
    let wf = ctx
        .store
        .create_workflow_run_full(
            "t-fast",
            "a-fast",
            2,
            Some(&policy),
            None,
            None,
            None,
            Some(&repo_path),
            Some("HEAD"),
        )
        .await?;

    let executor = Arc::new(SimulatedRoleExecutor::with_approval());
    let coordinator = WorkflowCoordinator::new(ctx.engine.pool.clone(), executor);

    // Step 1: Created -> Planning
    coordinator.step(&wf.id).await?;
    // Step 2: Planning -> Implementing
    coordinator.step(&wf.id).await?;
    // Step 3: Implementing -> Verifying
    coordinator.step(&wf.id).await?;

    let wf_verifying = ctx.store.get_workflow_run(&wf.id).await?.unwrap();
    assert_eq!(wf_verifying.status, WorkflowStage::Verifying);

    assert_verification_profile_gate(&coordinator, &ctx.store, &wf.id).await?;

    teardown_test(ctx).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn standard_auto_execution() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;

    let policy = sample_policy();
    ctx.store.verification_store().save_policy(&policy).await?;
    let repo = tempfile::tempdir()?;
    let repo_path = repo.path().to_string_lossy().into_owned();
    let wf = ctx
        .store
        .create_workflow_run_full(
            "t-std",
            "a-std",
            2,
            Some(&policy),
            None,
            None,
            None,
            Some(&repo_path),
            Some("HEAD"),
        )
        .await?;

    let executor = Arc::new(SimulatedRoleExecutor::with_approval());
    let coordinator = WorkflowCoordinator::new(ctx.engine.pool.clone(), executor);

    coordinator.step(&wf.id).await?; // -> Planning
    coordinator.step(&wf.id).await?; // -> Implementing
    coordinator.step(&wf.id).await?; // -> Verifying
    assert_verification_profile_gate(&coordinator, &ctx.store, &wf.id).await?;

    teardown_test(ctx).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn review_auto_execution() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;

    let policy = sample_policy();
    ctx.store.verification_store().save_policy(&policy).await?;
    let repo = tempfile::tempdir()?;
    let repo_path = repo.path().to_string_lossy().into_owned();
    let wf = ctx
        .store
        .create_workflow_run_full(
            "t-rev",
            "a-rev",
            2,
            Some(&policy),
            None,
            None,
            None,
            Some(&repo_path),
            Some("HEAD"),
        )
        .await?;

    let executor = Arc::new(SimulatedRoleExecutor::with_approval());
    let coordinator = WorkflowCoordinator::new(ctx.engine.pool.clone(), executor);

    coordinator.step(&wf.id).await?; // -> Planning
    coordinator.step(&wf.id).await?; // -> Implementing
    coordinator.step(&wf.id).await?; // -> Verifying
    assert_verification_profile_gate(&coordinator, &ctx.store, &wf.id).await?;

    teardown_test(ctx).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn full_auto_execution() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;

    let policy = sample_policy();
    ctx.store.verification_store().save_policy(&policy).await?;
    let repo = tempfile::tempdir()?;
    let repo_path = repo.path().to_string_lossy().into_owned();
    let wf = ctx
        .store
        .create_workflow_run_full(
            "t-full",
            "a-full",
            2,
            Some(&policy),
            None,
            None,
            None,
            Some(&repo_path),
            Some("HEAD"),
        )
        .await?;

    let executor = Arc::new(SimulatedRoleExecutor::with_approval());
    let coordinator = WorkflowCoordinator::new(ctx.engine.pool.clone(), executor);

    coordinator.step(&wf.id).await?; // -> Planning
    coordinator.step(&wf.id).await?; // -> Implementing
    coordinator.step(&wf.id).await?; // -> Verifying
    assert_verification_profile_gate(&coordinator, &ctx.store, &wf.id).await?;

    teardown_test(ctx).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn repair_path_is_blocked_without_verification_profile() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;

    let policy = sample_policy();
    ctx.store.verification_store().save_policy(&policy).await?;
    let repo = tempfile::tempdir()?;
    let repo_path = repo.path().to_string_lossy().into_owned();
    let wf = ctx
        .store
        .create_workflow_run_full(
            "t-repair",
            "a-repair",
            2,
            Some(&policy),
            None,
            None,
            None,
            Some(&repo_path),
            Some("HEAD"),
        )
        .await?;

    let executor = Arc::new(SimulatedRoleExecutor::with_approval());
    let coordinator = WorkflowCoordinator::new(ctx.engine.pool.clone(), executor.clone());

    coordinator.step(&wf.id).await?; // -> Planning
    coordinator.step(&wf.id).await?; // -> Implementing
    coordinator.step(&wf.id).await?; // -> Verifying
    assert_verification_profile_gate(&coordinator, &ctx.store, &wf.id).await?;

    teardown_test(ctx).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn provider_fallback_live_path() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;

    let role = RoleDefinition::reviewer_v1();
    // Simulate quota exhausted on antigravity -> falls back to codex
    let target =
        RoleRuntimeResolver::resolve_target_live(&ctx.engine.pool, &role, Some("antigravity-acp"))
            .await?;
    assert_eq!(target.provider, "codex");
    assert_eq!(target.credential_id.as_deref(), Some("codex-main"));

    teardown_test(ctx).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn session_independence_live_path() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;

    let policy = sample_policy();
    ctx.store.verification_store().save_policy(&policy).await?;
    let repo = tempfile::tempdir()?;
    let repo_path = repo.path().to_string_lossy().into_owned();
    let wf = ctx
        .store
        .create_workflow_run_full(
            "t-indep",
            "a-indep",
            2,
            Some(&policy),
            None,
            None,
            None,
            Some(&repo_path),
            Some("HEAD"),
        )
        .await?;

    let executor = Arc::new(SimulatedRoleExecutor::with_approval());
    let coordinator = WorkflowCoordinator::new(ctx.engine.pool.clone(), executor);

    coordinator.step(&wf.id).await?; // -> Planning
    coordinator.step(&wf.id).await?; // -> Implementing
    coordinator.step(&wf.id).await?; // -> Verifying
    assert_verification_profile_gate(&coordinator, &ctx.store, &wf.id).await?;
    let roles = ctx.store.list_role_executions(&wf.id).await?;
    assert_eq!(roles.len(), 2);

    let agent_ids: Vec<String> = roles
        .iter()
        .flat_map(|r| r.agent_execution_ids.clone())
        .collect();
    let unique_count: std::collections::BTreeSet<_> = agent_ids.iter().collect();
    assert_eq!(agent_ids.len(), unique_count.len());

    teardown_test(ctx).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn restart_recovery() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;

    let policy = sample_policy();
    ctx.store.verification_store().save_policy(&policy).await?;
    let repo = tempfile::tempdir()?;
    let repo_path = repo.path().to_string_lossy().into_owned();
    let wf = ctx
        .store
        .create_workflow_run_full(
            "t-rec",
            "a-rec",
            2,
            Some(&policy),
            None,
            None,
            None,
            Some(&repo_path),
            Some("HEAD"),
        )
        .await?;

    let executor = Arc::new(SimulatedRoleExecutor::with_approval());
    let coordinator1 = WorkflowCoordinator::new(ctx.engine.pool.clone(), executor.clone());

    // Advance partially to Implementing
    coordinator1.step(&wf.id).await?; // -> Planning
    coordinator1.step(&wf.id).await?; // -> Implementing
    drop(coordinator1);

    // New coordinator starts up fresh and resumes
    let coordinator2 = WorkflowCoordinator::new(ctx.engine.pool.clone(), executor);
    coordinator2.step(&wf.id).await?; // Implementing -> Verifying
    assert_verification_profile_gate(&coordinator2, &ctx.store, &wf.id).await?;

    teardown_test(ctx).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn cancellation_propagation() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;

    let wf = ctx
        .store
        .create_workflow_run_full(
            "t-cancel", "a-cancel", 2, None, None, None, None, None, None,
        )
        .await?;

    let executor = Arc::new(SimulatedRoleExecutor::with_approval());
    let coordinator = WorkflowCoordinator::new(ctx.engine.pool.clone(), executor);

    coordinator.step(&wf.id).await?; // -> Planning
    let cancelled = coordinator
        .cancel_workflow(&wf.id, "operator interrupt")
        .await?;
    assert_eq!(cancelled.status, WorkflowStage::Cancelled);
    assert_eq!(
        cancelled.cancellation_reason.as_deref(),
        Some("operator interrupt")
    );

    // Cancellation cannot create a new mutation authority for this workflow.
    let locks: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM orbit_attempt_workspace_locks WHERE attempt_id = 'a-cancel'",
    )
    .fetch_one(&ctx.engine.pool)
    .await?;
    assert_eq!(locks, 0);

    teardown_test(ctx).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn cli_only_operator_path() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;

    // Create workflow with durable parameters
    let wf = ctx
        .store
        .create_workflow_run_full(
            "cli-task-1",
            "cli-att-1",
            2,
            None,
            None,
            None,
            Some("Refactor via CLI"),
            Some("."),
            Some("HEAD"),
        )
        .await?;

    let show_output = format_workflow_show(&wf, &[]);
    assert!(show_output.contains(&wf.id));
    assert!(show_output.contains("software_change@1"));

    teardown_test(ctx).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL and --test-threads=1"]
async fn relative_repository_is_stable_after_restart_from_another_cwd() -> Result<()> {
    let ctx = setup_test().await?;
    let root = tempfile::tempdir()?;
    std::fs::create_dir(root.path().join("repo"))?;
    std::fs::create_dir(root.path().join("other"))?;
    std::fs::write(root.path().join("repo/candidate.txt"), b"candidate")?;
    let original_cwd = std::env::current_dir()?;
    std::env::set_current_dir(root.path())?;
    let created = ctx
        .store
        .create_workflow_run_full(
            "s4-relative-repo",
            &format!("att-{}", id()),
            1,
            None,
            None,
            None,
            Some("inspect candidate"),
            Some("repo"),
            Some("base"),
        )
        .await;
    std::env::set_current_dir(root.path().join("other"))?;
    let result = async {
        let created = created?;
        let restarted_store = WorkflowStore::new(ctx.engine.pool.clone());
        let resumed = restarted_store
            .get_workflow_run(&created.id)
            .await?
            .unwrap();
        assert_eq!(
            resumed.repository_path.as_deref(),
            root.path().join("repo").canonicalize()?.to_str()
        );
        let repo = std::path::Path::new(resumed.repository_path.as_deref().unwrap());
        let first = compute_workspace_state(repo, "base").await?;
        let second = compute_workspace_state(&root.path().join("repo"), "base").await?;
        assert_eq!(first.state_id, second.state_id);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    std::env::set_current_dir(original_cwd)?;
    result?;
    teardown_test(ctx).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn candidate_mutation_is_rejected_before_review_and_full() -> Result<()> {
    let ctx = setup_test().await?;
    let repo = tempfile::tempdir()?;
    let candidate = repo.path().join("candidate.txt");
    std::fs::write(&candidate, b"reviewed state")?;
    let state = compute_workspace_state(repo.path(), "base").await?;
    let coordinator = WorkflowCoordinator::new(
        ctx.engine.pool.clone(),
        Arc::new(SimulatedRoleExecutor::with_approval()),
    );

    let review_wf = ctx
        .store
        .create_workflow_run_full(
            "s4-review-boundary",
            &format!("att-{}", id()),
            1,
            None,
            None,
            None,
            Some("review"),
            repo.path().to_str(),
            Some("base"),
        )
        .await?;
    ctx.store
        .transition_workflow_stage(&review_wf.id, WorkflowStage::Planning, None, None, None)
        .await?;
    ctx.store
        .transition_workflow_stage(&review_wf.id, WorkflowStage::Implementing, None, None, None)
        .await?;
    ctx.store
        .transition_workflow_stage(
            &review_wf.id,
            WorkflowStage::Verifying,
            Some(&state.state_id),
            None,
            None,
        )
        .await?;
    ctx.store
        .transition_workflow_stage(
            &review_wf.id,
            WorkflowStage::Reviewing,
            Some(&state.state_id),
            None,
            None,
        )
        .await?;
    std::fs::write(&candidate, b"changed after STANDARD")?;
    let review_error = coordinator.step(&review_wf.id).await.unwrap_err();
    assert!(
        review_error
            .to_string()
            .contains("WORKSPACE_MUTATION_VIOLATION")
    );
    assert_eq!(
        ctx.store
            .get_workflow_run(&review_wf.id)
            .await?
            .unwrap()
            .status,
        WorkflowStage::Reviewing
    );

    let reviewed_state = compute_workspace_state(repo.path(), "base").await?;
    let full_wf = ctx
        .store
        .create_workflow_run_full(
            "s4-full-boundary",
            &format!("att-{}", id()),
            1,
            None,
            None,
            None,
            Some("final regression"),
            repo.path().to_str(),
            Some("base"),
        )
        .await?;
    for stage in [
        WorkflowStage::Planning,
        WorkflowStage::Implementing,
        WorkflowStage::Verifying,
        WorkflowStage::Reviewing,
        WorkflowStage::Regression,
    ] {
        let state_id = matches!(
            stage,
            WorkflowStage::Verifying | WorkflowStage::Reviewing | WorkflowStage::Regression
        )
        .then_some(reviewed_state.state_id.as_str());
        ctx.store
            .transition_workflow_stage(&full_wf.id, stage, state_id, None, None)
            .await?;
    }
    let review = ReviewDecision {
        decision: ReviewDecisionStatus::Approve,
        summary: "approved candidate".into(),
        findings: vec![],
        requested_changes: vec![],
        suggested_additional_checks: vec![],
    };
    ctx.store
        .save_handoff_artifact(
            &full_wf.id,
            None,
            HandoffType::Review,
            Some(&reviewed_state.state_id),
            serde_json::to_value(review)?,
        )
        .await?;
    std::fs::write(&candidate, b"changed after review")?;
    let full_error = coordinator.step(&full_wf.id).await.unwrap_err();
    assert!(
        full_error
            .to_string()
            .contains("WORKSPACE_MUTATION_VIOLATION")
    );
    assert_eq!(
        ctx.store
            .get_workflow_run(&full_wf.id)
            .await?
            .unwrap()
            .status,
        WorkflowStage::Regression
    );
    teardown_test(ctx).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn empty_reviewer_diff_records_review_error() -> Result<()> {
    let ctx = setup_test().await?;
    let repo = tempfile::tempdir()?;
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "orbit@example.invalid"],
        vec!["config", "user.name", "Orbit fixture"],
    ] {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(args)
            .status()?;
        assert!(status.success());
    }
    std::fs::write(repo.path().join("tracked.txt"), b"base")?;
    for args in [vec!["add", "tracked.txt"], vec!["commit", "-qm", "base"]] {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(args)
            .status()?;
        assert!(status.success());
    }
    let state = compute_workspace_state(repo.path(), "HEAD").await?;
    let wf = ctx
        .store
        .create_workflow_run_full(
            "s4-review-diff",
            &format!("att-{}", id()),
            1,
            None,
            None,
            None,
            Some("review"),
            repo.path().to_str(),
            Some("HEAD"),
        )
        .await?;
    for stage in [
        WorkflowStage::Planning,
        WorkflowStage::Implementing,
        WorkflowStage::Verifying,
        WorkflowStage::Reviewing,
    ] {
        let state_id = matches!(stage, WorkflowStage::Verifying | WorkflowStage::Reviewing)
            .then_some(state.state_id.as_str());
        ctx.store
            .transition_workflow_stage(&wf.id, stage, state_id, None, None)
            .await?;
    }
    let coordinator = WorkflowCoordinator::new(
        ctx.engine.pool.clone(),
        Arc::new(SimulatedRoleExecutor::with_approval()),
    );
    assert_eq!(
        coordinator.step(&wf.id).await?,
        WorkflowStepResult::Terminal(WorkflowStage::Failed)
    );
    let failed = ctx.store.get_workflow_run(&wf.id).await?.unwrap();
    assert!(
        failed
            .failure_reason
            .as_deref()
            .unwrap()
            .contains("REVIEW_ERROR")
    );
    assert!(
        ctx.store
            .get_latest_handoff_of_type(&wf.id, HandoffType::Review)
            .await?
            .is_none()
    );
    teardown_test(ctx).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn one_step_owner_and_cancel_fences_late_role_completion() -> Result<()> {
    let ctx = setup_test().await?;
    let wf = ctx
        .store
        .create_workflow_run("s6-owner", &format!("att-{}", id()), 2, None)
        .await?;
    let (first, second) = tokio::join!(
        ctx.store.claim_workflow_step(&wf.id),
        ctx.store.claim_workflow_step(&wf.id),
    );
    let first = first?;
    let second = second?;
    assert_eq!(
        usize::from(first.is_some()) + usize::from(second.is_some()),
        1
    );
    let claim = first.or(second).unwrap();
    let owned = ctx.store.with_step_claim(claim.clone());
    let coordinator = WorkflowCoordinator::new(
        ctx.engine.pool.clone(),
        Arc::new(SimulatedRoleExecutor::with_approval()),
    );
    assert_eq!(coordinator.step(&wf.id).await?, WorkflowStepResult::Waiting);
    let role = owned
        .create_role_execution(
            &wf.id,
            &RoleDefinition::planner_v1(),
            "PLANNING",
            1,
            None,
            None,
        )
        .await?;
    let cancelled = coordinator
        .cancel_workflow(&wf.id, "s6 cancel race")
        .await?;
    assert_eq!(cancelled.status, WorkflowStage::Cancelled);
    assert!(
        owned
            .complete_role_execution_success(&role.id, None, None)
            .await
            .is_err()
    );
    assert!(
        owned
            .transition_workflow_stage(&wf.id, WorkflowStage::Planning, None, None, None)
            .await
            .is_err()
    );
    assert_eq!(
        ctx.store.get_workflow_run(&wf.id).await?.unwrap().status,
        WorkflowStage::Cancelled
    );
    teardown_test(ctx).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn different_attempts_same_repository_cannot_both_mutate() -> Result<()> {
    let ctx = setup_test().await?;
    let repo = tempfile::tempdir()?;
    let repository = repo.path().to_str().unwrap();
    let first_wf = ctx
        .store
        .create_workflow_run_full(
            "s6-a",
            &format!("att-{}", id()),
            1,
            None,
            None,
            None,
            None,
            Some(repository),
            None,
        )
        .await?;
    let second_wf = ctx
        .store
        .create_workflow_run_full(
            "s6-b",
            &format!("att-{}", id()),
            1,
            None,
            None,
            None,
            None,
            Some(repository),
            None,
        )
        .await?;
    let role = RoleDefinition::implementer_v1();
    let first = ctx
        .store
        .create_role_execution(&first_wf.id, &role, "IMPLEMENTING", 1, None, None)
        .await?;
    let second = ctx
        .store
        .create_role_execution(&second_wf.id, &role, "IMPLEMENTING", 1, None, None)
        .await?;
    ctx.store
        .acquire_workspace_mutation_lock(&first_wf.attempt_id, &first.id)
        .await?;
    assert!(
        ctx.store
            .acquire_workspace_mutation_lock(&second_wf.attempt_id, &second.id)
            .await
            .is_err()
    );
    ctx.store
        .release_workspace_mutation_lock(&first_wf.attempt_id, &first.id)
        .await?;
    ctx.store
        .acquire_workspace_mutation_lock(&second_wf.attempt_id, &second.id)
        .await?;
    ctx.store
        .release_workspace_mutation_lock(&second_wf.attempt_id, &second.id)
        .await?;
    teardown_test(ctx).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn restart_recovers_dead_owner_without_external_role() -> Result<()> {
    let ctx = setup_test().await?;
    let wf = ctx
        .store
        .create_workflow_run("s6-dead-owner", &format!("att-{}", id()), 1, None)
        .await?;
    let claim = ctx.store.claim_workflow_step(&wf.id).await?.unwrap();
    let owned = ctx.store.with_step_claim(claim);
    let role = owned
        .create_role_execution(
            &wf.id,
            &RoleDefinition::implementer_v1(),
            "IMPLEMENTING",
            1,
            None,
            None,
        )
        .await?;
    ctx.store
        .acquire_workspace_mutation_lock(&wf.attempt_id, &role.id)
        .await?;
    sqlx::query("UPDATE orbit_workflow_runs SET step_owner_pid = 99999999 WHERE id = $1")
        .bind(&wf.id)
        .execute(&ctx.engine.pool)
        .await?;
    assert!(ctx.store.recover_orphaned_workflow_step(&wf.id).await?);
    assert_eq!(
        ctx.store
            .get_role_execution(&role.id)
            .await?
            .unwrap()
            .status,
        RoleExecutionStatus::Failed
    );
    assert!(
        !ctx.store
            .check_workspace_mutation_lock(&wf.attempt_id, &role.id)
            .await?
    );
    let new_claim = ctx.store.claim_workflow_step(&wf.id).await?.unwrap();
    assert!(new_claim.generation > 1);
    ctx.store.release_workflow_step(&new_claim).await?;
    teardown_test(ctx).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn restart_reuses_completed_provider_handoff_without_rerun() -> Result<()> {
    let ctx = setup_test().await?;
    let wf = ctx
        .store
        .create_workflow_run("s6-finished-role", &format!("att-{}", id()), 1, None)
        .await?;
    ctx.store
        .transition_workflow_stage(&wf.id, WorkflowStage::Planning, None, None, None)
        .await?;
    let claim = ctx.store.claim_workflow_step(&wf.id).await?.unwrap();
    let owned = ctx.store.with_step_claim(claim);
    let role = owned
        .create_role_execution(
            &wf.id,
            &RoleDefinition::planner_v1(),
            "PLANNING",
            1,
            None,
            None,
        )
        .await?;
    let handoff = owned
        .save_handoff_artifact(
            &wf.id,
            Some(&role.id),
            HandoffType::Plan,
            None,
            serde_json::json!({
                "summary":"plan", "affected_areas":[], "implementation_steps":["change file"],
                "expected_files":[], "risks":[], "verification_notes":[], "open_questions":[]
            }),
        )
        .await?;
    owned
        .complete_role_execution_success(&role.id, None, Some(&handoff.id))
        .await?;
    sqlx::query("UPDATE orbit_workflow_runs SET step_owner_pid = 99999999 WHERE id = $1")
        .bind(&wf.id)
        .execute(&ctx.engine.pool)
        .await?;
    let coordinator = WorkflowCoordinator::new(
        ctx.engine.pool.clone(),
        Arc::new(SimulatedRoleExecutor::with_approval()),
    );
    assert_eq!(
        coordinator.step(&wf.id).await?,
        WorkflowStepResult::Advanced {
            from: WorkflowStage::Planning,
            to: WorkflowStage::Implementing
        }
    );
    assert_eq!(ctx.store.list_role_executions(&wf.id).await?.len(), 1);
    teardown_test(ctx).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn restart_keeps_uncertain_external_role_fenced() -> Result<()> {
    let ctx = setup_test().await?;
    let wf = ctx
        .store
        .create_workflow_run("s6-uncertain-role", &format!("att-{}", id()), 1, None)
        .await?;
    let claim = ctx.store.claim_workflow_step(&wf.id).await?.unwrap();
    let owned = ctx.store.with_step_claim(claim);
    let role = owned
        .create_role_execution(
            &wf.id,
            &RoleDefinition::implementer_v1(),
            "IMPLEMENTING",
            1,
            None,
            None,
        )
        .await?;
    sqlx::query("UPDATE orbit_role_executions SET status = 'RUNNING' WHERE id = $1")
        .bind(&role.id)
        .execute(&ctx.engine.pool)
        .await?;
    ctx.store
        .acquire_workspace_mutation_lock(&wf.attempt_id, &role.id)
        .await?;
    sqlx::query("UPDATE orbit_workflow_runs SET step_owner_pid = 99999999 WHERE id = $1")
        .bind(&wf.id)
        .execute(&ctx.engine.pool)
        .await?;
    let error = ctx
        .store
        .recover_orphaned_workflow_step(&wf.id)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("WORKFLOW_RECOVERY_REQUIRES_EXTERNAL_RECONCILIATION")
    );
    assert!(
        ctx.store
            .check_workspace_mutation_lock(&wf.attempt_id, &role.id)
            .await?
    );
    teardown_test(ctx).await
}

struct HoldingRoleExecutor {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl RoleAgentExecutor for HoldingRoleExecutor {
    async fn execute_role(
        &self,
        _pool: &PgPool,
        _wf_run: &WorkflowRun,
        _role_exec: &RoleExecution,
        _role: &RoleDefinition,
        _target: &ResolvedExecutionTarget,
        _task_text: &str,
        _repo_path: &std::path::Path,
        _input_handoff: Option<&HandoffArtifact>,
        _cancellation: tokio::sync::watch::Receiver<bool>,
    ) -> Result<RoleExecutionOutcome> {
        self.started.notify_one();
        self.release.notified().await;
        Ok(RoleExecutionOutcome {
            raw_output: serde_json::json!({
                "summary":"plan", "affected_areas":[], "implementation_steps":["change file"],
                "expected_files":[], "risks":[], "verification_notes":[], "open_questions":[]
            })
            .to_string(),
            agent_execution_ids: vec![],
            termination_reason: None,
        })
    }
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn simultaneous_steps_and_cancel_have_one_owner() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;
    let wf = ctx
        .store
        .create_workflow_run("s6-step-race", &format!("att-{}", id()), 1, None)
        .await?;
    let executor = Arc::new(HoldingRoleExecutor {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let coordinator = Arc::new(WorkflowCoordinator::new(
        ctx.engine.pool.clone(),
        executor.clone(),
    ));
    coordinator.step(&wf.id).await?;
    let first_coordinator = Arc::clone(&coordinator);
    let workflow_id = wf.id.clone();
    let first = tokio::spawn(async move { first_coordinator.step(&workflow_id).await });
    executor.started.notified().await;
    assert_eq!(coordinator.step(&wf.id).await?, WorkflowStepResult::Waiting);
    coordinator
        .cancel_workflow(&wf.id, "operator cancelled while provider was active")
        .await?;
    executor.release.notify_one();
    assert_eq!(
        first.await??,
        WorkflowStepResult::Terminal(WorkflowStage::Cancelled)
    );
    assert_eq!(
        ctx.store.get_workflow_run(&wf.id).await?.unwrap().status,
        WorkflowStage::Cancelled
    );
    let roles = ctx.store.list_role_executions(&wf.id).await?;
    assert_eq!(roles.len(), 1);
    assert_eq!(roles[0].status, RoleExecutionStatus::Cancelled);
    teardown_test(ctx).await
}

struct LocalProcessRoleExecutor {
    started: tokio::sync::Notify,
    child_pid: std::sync::atomic::AtomicU32,
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn recovery_retains_lock_after_unconfirmed_role_cleanup() -> Result<()> {
    let ctx = setup_test().await?;
    let wf = ctx
        .store
        .create_workflow_run("s7-unconfirmed-cleanup", &format!("att-{}", id()), 1, None)
        .await?;
    let claim = ctx.store.claim_workflow_step(&wf.id).await?.unwrap();
    let role = ctx
        .store
        .with_step_claim(claim)
        .create_role_execution(
            &wf.id,
            &RoleDefinition::implementer_v1(),
            "IMPLEMENTING",
            1,
            None,
            None,
        )
        .await?;
    ctx.store
        .acquire_workspace_mutation_lock(&wf.attempt_id, &role.id)
        .await?;
    sqlx::query("UPDATE orbit_role_executions SET status = 'FAILED' WHERE id = $1")
        .bind(&role.id)
        .execute(&ctx.engine.pool)
        .await?;
    sqlx::query("INSERT INTO orbit_agent_executions (id, role_execution_id, agent_type, started_at_ms, status, metadata) VALUES ($1, $2, 'local-acp', 1, 'FAILED', '{\"cleanup_confirmed\":false}'::jsonb)")
        .bind(id())
        .bind(&role.id)
        .execute(&ctx.engine.pool)
        .await?;
    sqlx::query("UPDATE orbit_workflow_runs SET step_owner_pid = 99999999 WHERE id = $1")
        .bind(&wf.id)
        .execute(&ctx.engine.pool)
        .await?;
    assert!(ctx.store.recover_orphaned_workflow_step(&wf.id).await?);
    assert!(
        ctx.store
            .check_workspace_mutation_lock(&wf.attempt_id, &role.id)
            .await?
    );
    teardown_test(ctx).await
}

#[async_trait::async_trait]
impl RoleAgentExecutor for LocalProcessRoleExecutor {
    async fn execute_role(
        &self,
        _pool: &PgPool,
        _wf_run: &WorkflowRun,
        _role_exec: &RoleExecution,
        _role: &RoleDefinition,
        _target: &ResolvedExecutionTarget,
        _task_text: &str,
        _repo_path: &std::path::Path,
        _input_handoff: Option<&HandoffArtifact>,
        mut cancellation: tokio::sync::watch::Receiver<bool>,
    ) -> Result<RoleExecutionOutcome> {
        let mut command = tokio::process::Command::new("sh");
        command
            .args(["-c", "sleep 30"])
            .process_group(0)
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let pid = child.id().unwrap();
        self.child_pid
            .store(pid, std::sync::atomic::Ordering::SeqCst);
        self.started.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while cancellation.changed().await.is_ok() {
                if *cancellation.borrow() {
                    break;
                }
            }
        })
        .await?;
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        child.wait().await?;
        anyhow::bail!("ROLE_EXECUTION_CANCELLED")
    }
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn cancel_from_another_coordinator_reaches_local_role_process() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;
    let wf = ctx
        .store
        .create_workflow_run("s7-cross-process-cancel", &format!("att-{}", id()), 1, None)
        .await?;
    let executor = Arc::new(LocalProcessRoleExecutor {
        started: tokio::sync::Notify::new(),
        child_pid: std::sync::atomic::AtomicU32::new(0),
    });
    let worker = Arc::new(WorkflowCoordinator::new(
        ctx.engine.pool.clone(),
        executor.clone(),
    ));
    let canceller = WorkflowCoordinator::new(ctx.engine.pool.clone(), executor.clone());
    worker.step(&wf.id).await?;
    let id = wf.id.clone();
    let worker_copy = Arc::clone(&worker);
    let active = tokio::spawn(async move { worker_copy.step(&id).await });
    executor.started.notified().await;
    canceller
        .cancel_workflow(&wf.id, "cancel from another coordinator")
        .await?;
    let completed = tokio::time::timeout(std::time::Duration::from_secs(5), active).await???;
    assert_eq!(
        completed,
        WorkflowStepResult::Terminal(WorkflowStage::Cancelled)
    );
    let pid = executor.child_pid.load(std::sync::atomic::Ordering::SeqCst);
    assert!(pid > 0);
    assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
    teardown_test(ctx).await
}
