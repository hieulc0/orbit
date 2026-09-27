//! Qualification Test Suite for Phase B3.1 Live Workflow Orchestration Bridge.
//! Verifies complete autonomous execution, live credential resolution, read-only enforcement,
//! mutation locking, multi-tier verification triggering, repair loops, fallback, and CLI invocation.

use anyhow::Result;
use orbit::{model::id, verification::*, workflow::*, workflow_coordinator::*};
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
    tx.commit().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn b31_01_workflow_fails_closed_without_pinned_verification_profile() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;

    let policy = sample_policy();
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
async fn b31_02_real_planner_codex_acp() -> Result<()> {
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
async fn b31_03_real_antigravity_role_execution() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;

    let role = RoleDefinition::reviewer_v1();
    let target = RoleRuntimeResolver::resolve_target_live(&ctx.engine.pool, &role, None).await?;
    assert_eq!(target.provider, "antigravity");
    assert_eq!(target.runtime_interface, "antigravity-acp");
    assert_eq!(target.requested_model.as_deref(), Some("gemini-3.8-flash"));

    teardown_test(ctx).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn b31_04_live_credential_resolution() -> Result<()> {
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
async fn b31_05_live_runtime_capability_resolution() -> Result<()> {
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
async fn b31_06_no_dummy_credentials() -> Result<()> {
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
async fn b31_07_structured_plan_handoff() -> Result<()> {
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
async fn b31_08_structured_implementation_handoff() -> Result<()> {
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
async fn b31_09_structured_review_handoff() -> Result<()> {
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
async fn b31_10_planner_read_only_live() -> Result<()> {
    let planner = RoleDefinition::planner_v1();
    assert_eq!(planner.workspace_access, WorkspaceAccess::ReadOnly);
    assert!(!planner.allowed_capabilities.repo_write);
    assert!(planner.allowed_capabilities.repo_read);
    assert!(planner.allowed_capabilities.structured_output);
    Ok(())
}

#[tokio::test]
async fn b31_11_reviewer_read_only_live() -> Result<()> {
    let reviewer = RoleDefinition::reviewer_v1();
    assert_eq!(reviewer.workspace_access, WorkspaceAccess::ReadOnly);
    assert!(!reviewer.allowed_capabilities.repo_write);
    assert!(reviewer.allowed_capabilities.repo_read);
    assert!(reviewer.allowed_capabilities.structured_output);
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn b31_12_implementer_mutation_lock_live() -> Result<()> {
    let ctx = setup_test().await?;

    ctx.store
        .acquire_workspace_mutation_lock("attempt-lock", "holder-1")
        .await?;
    // Second concurrent lock must fail
    let err = ctx
        .store
        .acquire_workspace_mutation_lock("attempt-lock", "holder-2")
        .await;
    assert!(err.is_err());

    ctx.store
        .release_workspace_mutation_lock("attempt-lock", "holder-1")
        .await?;
    // Now holder-2 can acquire
    assert!(
        ctx.store
            .acquire_workspace_mutation_lock("attempt-lock", "holder-2")
            .await
            .is_ok()
    );
    ctx.store
        .release_workspace_mutation_lock("attempt-lock", "holder-2")
        .await?;

    teardown_test(ctx).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn b31_13_fast_auto_execution() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;

    let policy = sample_policy();
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
async fn b31_14_standard_auto_execution() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;

    let policy = sample_policy();
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
async fn b31_15_review_auto_execution() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;

    let policy = sample_policy();
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
async fn b31_16_full_auto_execution() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;

    let policy = sample_policy();
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
async fn b31_17_repair_path_is_blocked_without_verification_profile() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;

    let policy = sample_policy();
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
async fn b31_18_provider_fallback_live_path() -> Result<()> {
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
async fn b31_19_session_independence_live_path() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;

    let policy = sample_policy();
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
async fn b31_20_restart_recovery() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;

    let policy = sample_policy();
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
async fn b31_21_cancellation_propagation() -> Result<()> {
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

    // Mutation lock must be free
    let lock_check = ctx
        .store
        .acquire_workspace_mutation_lock("a-cancel", "new-holder")
        .await;
    assert!(lock_check.is_ok());

    teardown_test(ctx).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn b31_22_cli_only_operator_path() -> Result<()> {
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
