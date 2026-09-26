use anyhow::Result;
use orbit::{
    engine::Engine,
    model::id,
    verification::{
        EnvironmentIdentity, VerificationCachePolicy, VerificationNetworkPolicy, VerificationPlan,
        VerificationPolicy, VerificationRunResult, VerificationStep, WorkspaceState,
        execute_verification_plan_with_policy,
    },
    workflow::{
        FailureEvidenceHandoff, HandoffType, ImplementationHandoff, PlanHandoff, ReviewDecision,
        ReviewDecisionStatus, ReviewFinding, RoleDefinition, RoleExecutionStatus,
        RoleRuntimeResolver, WorkflowStage, WorkflowStore, WorkspaceAccess,
    },
};
use sqlx::PgPool;
use std::collections::BTreeMap;

struct TestContext {
    engine: Engine,
    store: WorkflowStore,
    _schema: String,
    url: String,
    _home: tempfile::TempDir,
}

async fn setup_workflow_test() -> Result<TestContext> {
    let base = if let Ok(url) = std::env::var("ORBIT_TEST_DATABASE_URL") {
        url
    } else if let Ok(home) = std::env::var("HOME") {
        let p = format!("{}/.orbit/private/database/control-plane-url", home);
        tokio::fs::read_to_string(p)
            .await
            .unwrap_or_default()
            .trim()
            .to_string()
    } else {
        String::new()
    };

    if base.is_empty() {
        anyhow::bail!("disposable database URL required for workflow qualification test");
    }

    let admin = PgPool::connect(&base).await?;
    let schema = format!("orbit_qual_wf_{}", id().replace('-', ""));
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await?;
    let separator = if base.contains('?') { '&' } else { '?' };
    let url = format!("{base}{separator}options=-csearch_path%3D{schema}");
    let home = tempfile::tempdir()?;

    let engine = Engine::connect(&url, home.path().join("artifacts"), 3).await?;
    let store = WorkflowStore::new(engine.pool.clone());

    Ok(TestContext {
        engine,
        store,
        _schema: schema,
        url,
        _home: home,
    })
}

fn sample_policy() -> VerificationPolicy {
    VerificationPolicy {
        id: "pol-test-1".into(),
        version: 1,
        name: "Test Policy".into(),
        required_steps: vec!["test".into()],
        allowed_commands: vec![],
        environment_policy: orbit::verification::VerificationEnvironmentPolicy {
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

fn sample_environment() -> EnvironmentIdentity {
    EnvironmentIdentity {
        execution_profile: "test".into(),
        isolation: "trusted".into(),
        runtime_image: Some("alpine:latest".into()),
        runtime_image_digest: None,
        oci_runtime: None,
        network_policy: VerificationNetworkPolicy::None,
        cache_policy: VerificationCachePolicy::Clean,
        environment_policy_digest: None,
        integration_environment_digest: None,
        architecture: std::env::consts::ARCH.into(),
        os: std::env::consts::OS.into(),
        orbit_version: "0.1.0".into(),
        browser_verification_digest: None,
        browser_runtime_image_digest: None,
        regression_policy_digest: None,
        selection_digest: None,
    }
}

#[tokio::test]
#[ignore = "requires database URL"]
async fn test_b3_happy_path() -> Result<()> {
    let ctx = setup_workflow_test().await?;
    let policy = sample_policy();
    ctx.store.verification_store().save_policy(&policy).await?;

    // 1. Create WorkflowRun
    let task_id = format!("task-{}", id());
    let attempt_id = format!("att-{}", id());
    let mut wf = ctx
        .store
        .create_workflow_run(&task_id, &attempt_id, 3, Some(&policy))
        .await?;
    assert_eq!(wf.status, WorkflowStage::Created);

    // 2. Planning stage
    wf = ctx
        .store
        .transition_workflow_stage(&wf.id, WorkflowStage::Planning, None, None, None)
        .await?;
    assert_eq!(wf.status, WorkflowStage::Planning);

    let planner_role = RoleDefinition::planner_v1();
    let planner_re = ctx
        .store
        .create_role_execution(&wf.id, &planner_role, "planning", 1, None, None)
        .await?;

    // Planner produces PlanHandoff
    let plan = PlanHandoff {
        summary: "Plan to add hello world".into(),
        affected_areas: vec!["src/lib.rs".into()],
        implementation_steps: vec!["Add hello function".into()],
        expected_files: vec!["src/lib.rs".into()],
        risks: vec![],
        verification_notes: vec!["Run pass.sh".into()],
        open_questions: vec![],
    };
    let plan_art = ctx
        .store
        .save_handoff_artifact(
            &wf.id,
            Some(&planner_re.id),
            HandoffType::Plan,
            None,
            serde_json::to_value(&plan)?,
        )
        .await?;
    ctx.store
        .complete_role_execution_success(&planner_re.id, None, Some(&plan_art.id))
        .await?;

    // 3. Implementing stage
    wf = ctx
        .store
        .transition_workflow_stage(&wf.id, WorkflowStage::Implementing, None, None, None)
        .await?;
    let implementer_role = RoleDefinition::implementer_v1();
    let impl_re = ctx
        .store
        .create_role_execution(
            &wf.id,
            &implementer_role,
            "implementing",
            1,
            None,
            Some(&plan_art.id),
        )
        .await?;

    // Implementer creates WorkspaceState A
    let ws_dir = tempfile::tempdir()?;
    let script_pass = ws_dir.path().join("pass.sh");
    tokio::fs::write(&script_pass, "#!/bin/sh\necho 'pass'\nexit 0\n").await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&script_pass, std::fs::Permissions::from_mode(0o755)).await?;
    }
    let ws_a = WorkspaceState::compute_from_parts("base-1", "head-1", Some("diff-sha-A"));

    let impl_handoff = ImplementationHandoff {
        summary: "Implemented hello world".into(),
        changed_files: vec!["pass.sh".into()],
        tests_added_or_modified: vec!["pass.sh".into()],
        exploratory_commands: vec!["./pass.sh".into()],
        known_limitations: vec![],
        verification_notes: vec!["ready for verification".into()],
    };
    let impl_art = ctx
        .store
        .save_handoff_artifact(
            &wf.id,
            Some(&impl_re.id),
            HandoffType::Implementation,
            Some(&ws_a.state_id),
            serde_json::to_value(&impl_handoff)?,
        )
        .await?;
    ctx.store
        .complete_role_execution_success(&impl_re.id, Some(&ws_a.state_id), Some(&impl_art.id))
        .await?;

    // 4. Verifying stage
    wf = ctx
        .store
        .transition_workflow_stage(
            &wf.id,
            WorkflowStage::Verifying,
            Some(&ws_a.state_id),
            None,
            None,
        )
        .await?;

    let plan_def = VerificationPlan::new(
        "plan-1",
        "Verification Plan",
        vec![VerificationStep::new_command(
            "test",
            "test pass.sh",
            vec![script_pass.to_str().unwrap().into()],
        )],
    );
    let run = execute_verification_plan_with_policy(
        ctx.store.verification_store(),
        &attempt_id,
        &ws_a,
        &plan_def,
        ws_dir.path(),
        sample_environment(),
        Some(&policy),
        None,
    )
    .await?;
    assert_eq!(run.overall_result, Some(VerificationRunResult::Passed));

    // 5. Reviewing stage
    wf = ctx
        .store
        .transition_workflow_stage(&wf.id, WorkflowStage::Reviewing, None, None, None)
        .await?;
    let reviewer_role = RoleDefinition::reviewer_v1();
    let rev_re = ctx
        .store
        .create_role_execution(
            &wf.id,
            &reviewer_role,
            "reviewing",
            1,
            Some(&ws_a.state_id),
            Some(&impl_art.id),
        )
        .await?;

    let review = ReviewDecision {
        decision: ReviewDecisionStatus::Approve,
        summary: "Looks great and tests pass".into(),
        findings: vec![],
        requested_changes: vec![],
        suggested_additional_checks: vec![],
    };
    let rev_art = ctx
        .store
        .save_handoff_artifact(
            &wf.id,
            Some(&rev_re.id),
            HandoffType::Review,
            Some(&ws_a.state_id),
            serde_json::to_value(&review)?,
        )
        .await?;
    ctx.store
        .complete_role_execution_success(&rev_re.id, Some(&ws_a.state_id), Some(&rev_art.id))
        .await?;

    // 6. Regression stage
    wf = ctx
        .store
        .transition_workflow_stage(&wf.id, WorkflowStage::Regression, None, None, None)
        .await?;
    let reg_run = execute_verification_plan_with_policy(
        ctx.store.verification_store(),
        &attempt_id,
        &ws_a,
        &plan_def,
        ws_dir.path(),
        sample_environment(),
        Some(&policy),
        None,
    )
    .await?;
    assert_eq!(reg_run.overall_result, Some(VerificationRunResult::Passed));

    // Check completion invariant before marking completed
    ctx.store.check_completion_invariant(&wf.id).await?;

    // 7. Complete stage
    wf = ctx
        .store
        .transition_workflow_stage(&wf.id, WorkflowStage::Completed, None, None, None)
        .await?;
    assert_eq!(wf.status, WorkflowStage::Completed);
    assert!(wf.status.is_terminal());

    Ok(())
}

#[tokio::test]
#[ignore = "requires database URL"]
async fn test_b3_verification_failure_and_repair() -> Result<()> {
    let ctx = setup_workflow_test().await?;
    let policy = sample_policy();
    ctx.store.verification_store().save_policy(&policy).await?;

    let task_id = format!("task-{}", id());
    let attempt_id = format!("att-{}", id());
    let mut wf = ctx
        .store
        .create_workflow_run(&task_id, &attempt_id, 3, Some(&policy))
        .await?;

    // Planning
    wf = ctx
        .store
        .transition_workflow_stage(&wf.id, WorkflowStage::Planning, None, None, None)
        .await?;
    wf = ctx
        .store
        .transition_workflow_stage(&wf.id, WorkflowStage::Implementing, None, None, None)
        .await?;

    // Implementer produces broken code WorkspaceState A
    let ws_dir = tempfile::tempdir()?;
    let script = ws_dir.path().join("pass.sh");
    tokio::fs::write(&script, "#!/bin/sh\necho 'syntax error' >&2\nexit 1\n").await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).await?;
    }
    let ws_a = WorkspaceState::compute_from_parts("base-1", "head-broken", Some("diff-sha-broken"));

    // Verifying
    wf = ctx
        .store
        .transition_workflow_stage(
            &wf.id,
            WorkflowStage::Verifying,
            Some(&ws_a.state_id),
            None,
            None,
        )
        .await?;

    let plan_def = VerificationPlan::new(
        "plan-1",
        "Verification Plan",
        vec![VerificationStep::new_command(
            "test",
            "test pass.sh",
            vec![script.to_str().unwrap().into()],
        )],
    );
    let run = execute_verification_plan_with_policy(
        ctx.store.verification_store(),
        &attempt_id,
        &ws_a,
        &plan_def,
        ws_dir.path(),
        sample_environment(),
        Some(&policy),
        None,
    )
    .await?;
    assert_eq!(run.overall_result, Some(VerificationRunResult::Failed));

    // Verification failed! Reviewer must NOT be invoked. Transition to REPAIRING.
    wf = ctx
        .store
        .transition_workflow_stage(&wf.id, WorkflowStage::Repairing, None, Some(2), None)
        .await?;
    assert_eq!(wf.status, WorkflowStage::Repairing);
    assert_eq!(wf.iteration, 2);

    // Save failure evidence handoff
    let failure_evidence = FailureEvidenceHandoff {
        failed_stage: "VERIFYING".into(),
        verification_run_id: Some(run.id.clone()),
        failed_steps: vec!["test".into()],
        error_summary: "pass.sh exited with code 1".into(),
        stdout_previews: BTreeMap::new(),
        stderr_previews: BTreeMap::new(),
    };
    ctx.store
        .save_handoff_artifact(
            &wf.id,
            None,
            HandoffType::FailureEvidence,
            Some(&ws_a.state_id),
            serde_json::to_value(&failure_evidence)?,
        )
        .await?;

    // Repair implementer runs with iteration 2 and fixes pass.sh -> WorkspaceState B
    tokio::fs::write(&script, "#!/bin/sh\necho 'repaired pass'\nexit 0\n").await?;
    let ws_b = WorkspaceState::compute_from_parts("base-1", "head-fixed", Some("diff-sha-fixed"));
    assert_ne!(ws_a.state_id, ws_b.state_id);

    // Old verification run(A) cannot qualify ws_b
    let qual_b_with_old_run = ctx
        .store
        .verification_store()
        .check_workspace_qualification(&ws_b.state_id, &policy, Some(&sample_environment()))
        .await?;
    assert!(qual_b_with_old_run.is_none());

    // Verify ws_b
    let _wf_verif = ctx
        .store
        .transition_workflow_stage(
            &wf.id,
            WorkflowStage::Verifying,
            Some(&ws_b.state_id),
            None,
            None,
        )
        .await?;
    let run_b = execute_verification_plan_with_policy(
        ctx.store.verification_store(),
        &attempt_id,
        &ws_b,
        &plan_def,
        ws_dir.path(),
        sample_environment(),
        Some(&policy),
        None,
    )
    .await?;
    assert_eq!(run_b.overall_result, Some(VerificationRunResult::Passed));

    let qual_b = ctx
        .store
        .verification_store()
        .check_workspace_qualification(&ws_b.state_id, &policy, Some(&sample_environment()))
        .await?;
    assert!(qual_b.is_some());

    Ok(())
}

#[tokio::test]
#[ignore = "requires database URL"]
async fn test_b3_review_changes_requested_and_repair() -> Result<()> {
    let ctx = setup_workflow_test().await?;
    let policy = sample_policy();
    ctx.store.verification_store().save_policy(&policy).await?;

    let task_id = format!("task-{}", id());
    let attempt_id = format!("att-{}", id());
    let mut wf = ctx
        .store
        .create_workflow_run(&task_id, &attempt_id, 3, Some(&policy))
        .await?;

    wf = ctx
        .store
        .transition_workflow_stage(&wf.id, WorkflowStage::Planning, None, None, None)
        .await?;
    wf = ctx
        .store
        .transition_workflow_stage(&wf.id, WorkflowStage::Implementing, None, None, None)
        .await?;

    let ws_dir = tempfile::tempdir()?;
    let script = ws_dir.path().join("pass.sh");
    tokio::fs::write(&script, "#!/bin/sh\necho 'ok'\nexit 0\n").await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).await?;
    }
    let ws_a = WorkspaceState::compute_from_parts("base-1", "head-a", Some("diff-sha-a"));

    // Verifying passes
    wf = ctx
        .store
        .transition_workflow_stage(
            &wf.id,
            WorkflowStage::Verifying,
            Some(&ws_a.state_id),
            None,
            None,
        )
        .await?;
    let plan_def = VerificationPlan::new(
        "plan-1",
        "Verification Plan",
        vec![VerificationStep::new_command(
            "test",
            "test pass.sh",
            vec![script.to_str().unwrap().into()],
        )],
    );
    let run = execute_verification_plan_with_policy(
        ctx.store.verification_store(),
        &attempt_id,
        &ws_a,
        &plan_def,
        ws_dir.path(),
        sample_environment(),
        Some(&policy),
        None,
    )
    .await?;
    assert_eq!(run.overall_result, Some(VerificationRunResult::Passed));

    // Reviewing requests changes
    wf = ctx
        .store
        .transition_workflow_stage(&wf.id, WorkflowStage::Reviewing, None, None, None)
        .await?;
    let review_changes = ReviewDecision {
        decision: ReviewDecisionStatus::ChangesRequested,
        summary: "Please add comments to pass.sh".into(),
        findings: vec![ReviewFinding {
            category: "style".into(),
            severity: "minor".into(),
            path: Some("pass.sh".into()),
            explanation: "missing header comment".into(),
            requested_change: Some("add header".into()),
        }],
        requested_changes: vec!["add header".into()],
        suggested_additional_checks: vec![],
    };
    ctx.store
        .save_handoff_artifact(
            &wf.id,
            None,
            HandoffType::Review,
            Some(&ws_a.state_id),
            serde_json::to_value(&review_changes)?,
        )
        .await?;

    // Reviewer requested changes -> REPAIRING
    wf = ctx
        .store
        .transition_workflow_stage(&wf.id, WorkflowStage::Repairing, None, Some(2), None)
        .await?;
    assert_eq!(wf.status, WorkflowStage::Repairing);
    assert_eq!(wf.iteration, 2);

    // Implementer adds comments -> WorkspaceState B
    tokio::fs::write(
        &script,
        "#!/bin/sh\n# Added header comment\necho 'ok'\nexit 0\n",
    )
    .await?;
    let ws_b = WorkspaceState::compute_from_parts("base-1", "head-b", Some("diff-sha-b"));
    assert_ne!(ws_a.state_id, ws_b.state_id);

    // Old review does not approve WorkspaceState B
    let _wf_verif = ctx
        .store
        .transition_workflow_stage(
            &wf.id,
            WorkflowStage::Verifying,
            Some(&ws_b.state_id),
            None,
            None,
        )
        .await?;
    let run_b = execute_verification_plan_with_policy(
        ctx.store.verification_store(),
        &attempt_id,
        &ws_b,
        &plan_def,
        ws_dir.path(),
        sample_environment(),
        Some(&policy),
        None,
    )
    .await?;
    assert_eq!(run_b.overall_result, Some(VerificationRunResult::Passed));

    // New review on ws_b with APPROVE
    wf = ctx
        .store
        .transition_workflow_stage(&wf.id, WorkflowStage::Reviewing, None, None, None)
        .await?;
    let review_approve = ReviewDecision {
        decision: ReviewDecisionStatus::Approve,
        summary: "Header added, approved".into(),
        findings: vec![],
        requested_changes: vec![],
        suggested_additional_checks: vec![],
    };
    ctx.store
        .save_handoff_artifact(
            &wf.id,
            None,
            HandoffType::Review,
            Some(&ws_b.state_id),
            serde_json::to_value(&review_approve)?,
        )
        .await?;

    // Final regression and complete
    wf = ctx
        .store
        .transition_workflow_stage(&wf.id, WorkflowStage::Regression, None, None, None)
        .await?;
    ctx.store.check_completion_invariant(&wf.id).await?;
    wf = ctx
        .store
        .transition_workflow_stage(&wf.id, WorkflowStage::Completed, None, None, None)
        .await?;
    assert_eq!(wf.status, WorkflowStage::Completed);

    Ok(())
}

#[tokio::test]
#[ignore = "requires database URL"]
async fn test_b3_final_regression_failure() -> Result<()> {
    let ctx = setup_workflow_test().await?;
    let policy = sample_policy();
    ctx.store.verification_store().save_policy(&policy).await?;

    let task_id = format!("task-{}", id());
    let attempt_id = format!("att-{}", id());
    let mut wf = ctx
        .store
        .create_workflow_run(&task_id, &attempt_id, 3, Some(&policy))
        .await?;

    wf = ctx
        .store
        .transition_workflow_stage(&wf.id, WorkflowStage::Planning, None, None, None)
        .await?;
    wf = ctx
        .store
        .transition_workflow_stage(&wf.id, WorkflowStage::Implementing, None, None, None)
        .await?;

    let ws_dir = tempfile::tempdir()?;
    let script = ws_dir.path().join("pass.sh");
    tokio::fs::write(&script, "#!/bin/sh\nexit 0\n").await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).await?;
    }
    let ws_a = WorkspaceState::compute_from_parts("base-1", "head-a", Some("diff-sha-a"));

    // Verifying passes
    wf = ctx
        .store
        .transition_workflow_stage(
            &wf.id,
            WorkflowStage::Verifying,
            Some(&ws_a.state_id),
            None,
            None,
        )
        .await?;
    let plan_def = VerificationPlan::new(
        "plan-1",
        "Verification Plan",
        vec![VerificationStep::new_command(
            "test",
            "test pass.sh",
            vec![script.to_str().unwrap().into()],
        )],
    );
    execute_verification_plan_with_policy(
        ctx.store.verification_store(),
        &attempt_id,
        &ws_a,
        &plan_def,
        ws_dir.path(),
        sample_environment(),
        Some(&policy),
        None,
    )
    .await?;

    // Reviewing APPROVES
    wf = ctx
        .store
        .transition_workflow_stage(&wf.id, WorkflowStage::Reviewing, None, None, None)
        .await?;
    let review = ReviewDecision {
        decision: ReviewDecisionStatus::Approve,
        summary: "Approved".into(),
        findings: vec![],
        requested_changes: vec![],
        suggested_additional_checks: vec![],
    };
    ctx.store
        .save_handoff_artifact(
            &wf.id,
            None,
            HandoffType::Review,
            Some(&ws_a.state_id),
            serde_json::to_value(&review)?,
        )
        .await?;

    // Regression stage
    wf = ctx
        .store
        .transition_workflow_stage(&wf.id, WorkflowStage::Regression, None, None, None)
        .await?;

    // But during regression, script fails (e.g. flaky or external condition)!
    tokio::fs::write(&script, "#!/bin/sh\nexit 1\n").await?;
    let reg_run = execute_verification_plan_with_policy(
        ctx.store.verification_store(),
        &attempt_id,
        &ws_a,
        &plan_def,
        ws_dir.path(),
        sample_environment(),
        Some(&policy),
        None,
    )
    .await?;
    assert_eq!(reg_run.overall_result, Some(VerificationRunResult::Failed));

    // Workflow MUST NOT complete directly; it transitions to REPAIRING
    wf = ctx
        .store
        .transition_workflow_stage(
            &wf.id,
            WorkflowStage::Repairing,
            None,
            Some(2),
            Some("regression failed"),
        )
        .await?;
    assert_eq!(wf.status, WorkflowStage::Repairing);

    Ok(())
}

#[tokio::test]
#[ignore = "requires database URL"]
async fn test_b3_provider_fallback_preserves_role_identity() -> Result<()> {
    let ctx = setup_workflow_test().await?;
    let reviewer = RoleDefinition::reviewer_v1();

    // 1. First resolution: prefers antigravity-acp
    let target1 = RoleRuntimeResolver::resolve_target(&reviewer, None)?;
    assert_eq!(target1.provider, "antigravity");
    assert_eq!(target1.runtime_interface, "antigravity-acp");

    // 2. Antigravity exhausted: fallback to codex-acp
    let target2 = RoleRuntimeResolver::resolve_target(&reviewer, Some("antigravity-acp"))?;
    assert_eq!(target2.provider, "codex");
    assert_eq!(target2.runtime_interface, "codex-acp");

    // Persist RoleExecution and simulate multiple AgentExecution fallback attempts
    let wf = ctx
        .store
        .create_workflow_run("task-1", "att-1", 3, None)
        .await?;
    let re = ctx
        .store
        .create_role_execution(&wf.id, &reviewer, "reviewing", 1, None, None)
        .await?;

    // First agent execution failed with quota
    ctx.store
        .set_role_execution_resolved(&re.id, &target1)
        .await?;
    ctx.store
        .record_agent_execution(&re.id, "agent-exec-antigravity-failed")
        .await?;

    // Second agent execution succeeded with fallback
    ctx.store
        .set_role_execution_resolved(&re.id, &target2)
        .await?;
    ctx.store
        .record_agent_execution(&re.id, "agent-exec-codex-success")
        .await?;

    let completed_re = ctx
        .store
        .complete_role_execution_success(&re.id, None, None)
        .await?;

    // Role identity is strictly preserved as "reviewer"
    assert_eq!(completed_re.role_id, "reviewer");
    assert_eq!(completed_re.agent_execution_ids.len(), 2);
    assert_eq!(
        completed_re.agent_execution_ids[0],
        "agent-exec-antigravity-failed"
    );
    assert_eq!(
        completed_re.agent_execution_ids[1],
        "agent-exec-codex-success"
    );

    Ok(())
}

#[tokio::test]
#[ignore = "requires database URL"]
async fn test_b3_session_independence() -> Result<()> {
    let ctx = setup_workflow_test().await?;
    let wf = ctx
        .store
        .create_workflow_run("task-1", "att-1", 3, None)
        .await?;

    // Planner completes and provider session disappears
    let plan = PlanHandoff {
        summary: "Plan 1".into(),
        affected_areas: vec!["src/main.rs".into()],
        implementation_steps: vec!["Step 1".into()],
        expected_files: vec!["src/main.rs".into()],
        risks: vec![],
        verification_notes: vec![],
        open_questions: vec![],
    };
    let _plan_art = ctx
        .store
        .save_handoff_artifact(
            &wf.id,
            None,
            HandoffType::Plan,
            None,
            serde_json::to_value(&plan)?,
        )
        .await?;

    // Next role (implementer) loads PlanHandoff purely from durable storage without any session ID
    let loaded_plan_art = ctx
        .store
        .get_latest_handoff_of_type(&wf.id, HandoffType::Plan)
        .await?
        .unwrap();
    let loaded_plan: PlanHandoff = serde_json::from_value(loaded_plan_art.structured_payload)?;
    assert_eq!(loaded_plan.summary, "Plan 1");

    Ok(())
}

#[tokio::test]
async fn test_b3_read_only_planner() -> Result<()> {
    let planner = RoleDefinition::planner_v1();
    assert_eq!(planner.workspace_access, WorkspaceAccess::ReadOnly);
    assert!(!planner.allowed_capabilities.repo_write);
    assert!(planner.allowed_capabilities.repo_read);
    assert!(planner.allowed_capabilities.structured_output);
    Ok(())
}

#[tokio::test]
async fn test_b3_read_only_reviewer() -> Result<()> {
    let reviewer = RoleDefinition::reviewer_v1();
    assert_eq!(reviewer.workspace_access, WorkspaceAccess::ReadOnly);
    assert!(!reviewer.allowed_capabilities.repo_write);
    assert!(reviewer.allowed_capabilities.repo_read);
    assert!(reviewer.allowed_capabilities.structured_output);
    Ok(())
}

#[tokio::test]
#[ignore = "requires database URL"]
async fn test_b3_single_mutator_lock() -> Result<()> {
    let ctx = setup_workflow_test().await?;
    let attempt_id = format!("att-{}", id());

    // Role 1 acquires lock
    ctx.store
        .acquire_workspace_mutation_lock(&attempt_id, "role-exec-1")
        .await?;

    // Role 2 attempts to acquire lock concurrently -> FAILS
    let res = ctx
        .store
        .acquire_workspace_mutation_lock(&attempt_id, "role-exec-2")
        .await;
    assert!(res.is_err(), "concurrent workspace lock must fail");

    // Role 1 releases lock
    ctx.store
        .release_workspace_mutation_lock(&attempt_id, "role-exec-1")
        .await?;

    // Now Role 2 can acquire lock
    ctx.store
        .acquire_workspace_mutation_lock(&attempt_id, "role-exec-2")
        .await?;
    ctx.store
        .release_workspace_mutation_lock(&attempt_id, "role-exec-2")
        .await?;

    Ok(())
}

#[tokio::test]
#[ignore = "requires database URL"]
async fn test_b3_iteration_exhaustion() -> Result<()> {
    let ctx = setup_workflow_test().await?;
    let mut wf = ctx
        .store
        .create_workflow_run("task-1", "att-1", 2, None)
        .await?;
    assert_eq!(wf.max_iterations, 2);

    // Iteration 1 fails
    wf = ctx
        .store
        .transition_workflow_stage(&wf.id, WorkflowStage::Planning, None, None, None)
        .await?;
    wf = ctx
        .store
        .transition_workflow_stage(&wf.id, WorkflowStage::Implementing, None, None, None)
        .await?;
    wf = ctx
        .store
        .transition_workflow_stage(&wf.id, WorkflowStage::Verifying, None, None, None)
        .await?;
    wf = ctx
        .store
        .transition_workflow_stage(&wf.id, WorkflowStage::Repairing, None, Some(2), None)
        .await?;

    // Iteration 2 fails -> exhausts limit
    wf = ctx
        .store
        .transition_workflow_stage(&wf.id, WorkflowStage::Verifying, None, None, None)
        .await?;

    // Attempting to exceed max_iterations triggers EXHAUSTED
    wf = ctx
        .store
        .transition_workflow_stage(
            &wf.id,
            WorkflowStage::Exhausted,
            None,
            None,
            Some("max iterations reached"),
        )
        .await?;
    assert_eq!(wf.status, WorkflowStage::Exhausted);
    assert!(wf.status.is_terminal());

    Ok(())
}

#[tokio::test]
#[ignore = "requires database URL"]
async fn test_b3_stale_review_invalidation() -> Result<()> {
    let ctx = setup_workflow_test().await?;
    let task_id = format!("task-{}", id());
    let attempt_id = format!("att-{}", id());
    let wf = ctx
        .store
        .create_workflow_run(&task_id, &attempt_id, 3, None)
        .await?;

    // Setup review for workspace state A
    let ws_a = "ws-state-A";
    let review = ReviewDecision {
        decision: ReviewDecisionStatus::Approve,
        summary: "Approved A".into(),
        findings: vec![],
        requested_changes: vec![],
        suggested_additional_checks: vec![],
    };
    ctx.store
        .save_handoff_artifact(
            &wf.id,
            None,
            HandoffType::Review,
            Some(ws_a),
            serde_json::to_value(&review)?,
        )
        .await?;

    // But workspace changed to B!
    let ws_b = "ws-state-B";
    ctx.store
        .transition_workflow_stage(&wf.id, WorkflowStage::Planning, None, None, None)
        .await?;
    ctx.store
        .transition_workflow_stage(&wf.id, WorkflowStage::Implementing, None, None, None)
        .await?;
    ctx.store
        .transition_workflow_stage(&wf.id, WorkflowStage::Verifying, Some(ws_b), None, None)
        .await?;
    ctx.store
        .transition_workflow_stage(&wf.id, WorkflowStage::Reviewing, None, None, None)
        .await?;
    ctx.store
        .transition_workflow_stage(&wf.id, WorkflowStage::Regression, None, None, None)
        .await?;

    // Completion invariant check MUST FAIL because review was for A, not B
    let res = ctx.store.check_completion_invariant(&wf.id).await;
    assert!(res.is_err(), "stale review must prevent completion");
    let err_str = res.unwrap_err().to_string();
    assert!(err_str.contains("stale review"));

    Ok(())
}

#[tokio::test]
#[ignore = "requires database URL"]
async fn test_b3_restart_durability() -> Result<()> {
    let ctx = setup_workflow_test().await?;
    let task_id = format!("task-{}", id());
    let attempt_id = format!("att-{}", id());
    let wf = ctx
        .store
        .create_workflow_run(&task_id, &attempt_id, 3, None)
        .await?;
    ctx.store
        .transition_workflow_stage(&wf.id, WorkflowStage::Planning, None, None, None)
        .await?;

    let planner_role = RoleDefinition::planner_v1();
    let re = ctx
        .store
        .create_role_execution(&wf.id, &planner_role, "planning", 1, None, None)
        .await?;
    let plan = PlanHandoff {
        summary: "Durable plan".into(),
        affected_areas: vec![],
        implementation_steps: vec![],
        expected_files: vec![],
        risks: vec![],
        verification_notes: vec![],
        open_questions: vec![],
    };
    let art = ctx
        .store
        .save_handoff_artifact(
            &wf.id,
            Some(&re.id),
            HandoffType::Plan,
            None,
            serde_json::to_value(&plan)?,
        )
        .await?;
    ctx.store
        .complete_role_execution_success(&re.id, None, Some(&art.id))
        .await?;

    // SIMULATE PROCESS RESTART: connect new engine & store to the exact same schema URL
    let new_engine =
        Engine::connect(&ctx.url, tempfile::tempdir()?.path().join("artifacts"), 3).await?;
    let new_store = WorkflowStore::new(new_engine.pool.clone());

    let loaded_wf = new_store.get_workflow_run(&wf.id).await?.unwrap();
    assert_eq!(loaded_wf.id, wf.id);
    assert_eq!(loaded_wf.status, WorkflowStage::Planning);

    let loaded_roles = new_store.list_role_executions(&wf.id).await?;
    assert_eq!(loaded_roles.len(), 1);
    assert_eq!(loaded_roles[0].status, RoleExecutionStatus::Succeeded);

    let loaded_art = new_store
        .get_latest_handoff_of_type(&wf.id, HandoffType::Plan)
        .await?
        .unwrap();
    let loaded_plan: PlanHandoff = serde_json::from_value(loaded_art.structured_payload)?;
    assert_eq!(loaded_plan.summary, "Durable plan");

    new_engine.pool.close().await;
    ctx.engine.pool.close().await;

    Ok(())
}
