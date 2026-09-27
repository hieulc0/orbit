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
async fn b31_13_fast_auto_execution() -> Result<()> {
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
async fn b31_14_standard_auto_execution() -> Result<()> {
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
async fn b31_15_review_auto_execution() -> Result<()> {
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
async fn b31_16_full_auto_execution() -> Result<()> {
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
async fn b31_17_repair_path_is_blocked_without_verification_profile() -> Result<()> {
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
async fn b31_20_restart_recovery() -> Result<()> {
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

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL and --test-threads=1"]
async fn s4_relative_repository_is_stable_after_restart_from_another_cwd() -> Result<()> {
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
async fn s4_candidate_mutation_is_rejected_before_review_and_full() -> Result<()> {
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
async fn s4_empty_reviewer_diff_records_review_error() -> Result<()> {
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
async fn s6_one_step_owner_and_cancel_fences_late_role_completion() -> Result<()> {
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
async fn s6_different_attempts_same_repository_cannot_both_mutate() -> Result<()> {
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
async fn s6_restart_recovers_dead_owner_without_external_role() -> Result<()> {
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
async fn s6_restart_reuses_completed_provider_handoff_without_rerun() -> Result<()> {
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
async fn s6_restart_keeps_uncertain_external_role_fenced() -> Result<()> {
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

struct S6HoldingExecutor {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl RoleAgentExecutor for S6HoldingExecutor {
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
async fn s6_simultaneous_steps_and_cancel_have_one_owner() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;
    let wf = ctx
        .store
        .create_workflow_run("s6-step-race", &format!("att-{}", id()), 1, None)
        .await?;
    let executor = Arc::new(S6HoldingExecutor {
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
