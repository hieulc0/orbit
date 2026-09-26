use anyhow::Result;
use orbit::{
    engine::Engine,
    model::id,
    regression_strategy::{
        ComponentMapping, CostClass, RegressionPolicy, RegressionStore, SelectionPolicy,
        SelectionReason, VerificationCheck, VerificationTier, select_verification,
    },
    verification::{
        EnvironmentIdentity, VerificationCachePolicy, VerificationNetworkPolicy,
        VerificationRunResult, VerificationStore, WorkspaceState,
    },
    workflow::{WorkflowStage, WorkflowStore},
};
use sqlx::PgPool;
use std::collections::BTreeMap;

struct TestContext {
    _engine: Engine,
    store: VerificationStore,
    workflow_store: WorkflowStore,
    reg_store: RegressionStore,
    _schema: String,
    url: String,
    _home: tempfile::TempDir,
}

async fn setup_regression_test() -> Result<TestContext> {
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
        anyhow::bail!("disposable database URL required for regression qualification test");
    }

    let admin = PgPool::connect(&base).await?;
    let schema = format!("orbit_qual_reg_{}", id().replace('-', ""));
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await?;
    let separator = if base.contains('?') { '&' } else { '?' };
    let url = format!("{base}{separator}options=-csearch_path%3D{schema}");
    let home = tempfile::tempdir()?;

    let engine = Engine::connect(&url, home.path().join("artifacts"), 3).await?;
    let store = VerificationStore::new(engine.pool.clone());
    let workflow_store = WorkflowStore::new(engine.pool.clone());
    let reg_store = RegressionStore::new(engine.pool.clone());

    Ok(TestContext {
        _engine: engine,
        store,
        workflow_store,
        reg_store,
        _schema: schema,
        url,
        _home: home,
    })
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

fn sample_selection_policy() -> SelectionPolicy {
    let mut pol = SelectionPolicy::new("sel-policy-1", "Test Selection Policy");
    pol.checks = vec![
        VerificationCheck {
            check_id: "format-check".into(),
            name: "Format Check".into(),
            tiers: vec![
                VerificationTier::Fast,
                VerificationTier::Standard,
                VerificationTier::Full,
            ],
            paths: vec!["**/*".into()],
            affected_components: vec!["core".into()],
            cost_class: CostClass::Cheap,
            required: true,
            always_run: true,
            dependencies: vec![],
            estimated_duration_ms: None,
            command: None,
            integration_environment_spec: None,
            browser_test_spec: None,
        },
        VerificationCheck {
            check_id: "auth-unit".into(),
            name: "Auth Unit Tests".into(),
            tiers: vec![
                VerificationTier::Fast,
                VerificationTier::Standard,
                VerificationTier::Full,
            ],
            paths: vec!["src/auth/**".into()],
            affected_components: vec!["auth".into()],
            cost_class: CostClass::Cheap,
            required: true,
            always_run: false,
            dependencies: vec![],
            estimated_duration_ms: None,
            command: None,
            integration_environment_spec: None,
            browser_test_spec: None,
        },
        VerificationCheck {
            check_id: "auth-integration".into(),
            name: "Auth Integration Tests".into(),
            tiers: vec![VerificationTier::Standard, VerificationTier::Full],
            paths: vec!["src/auth/**".into(), "tests/auth/**".into()],
            affected_components: vec!["auth".into(), "api".into()],
            cost_class: CostClass::Medium,
            required: true,
            always_run: false,
            dependencies: vec!["auth-unit".into()],
            estimated_duration_ms: None,
            command: None,
            integration_environment_spec: None,
            browser_test_spec: None,
        },
        VerificationCheck {
            check_id: "ui-lint".into(),
            name: "UI Lint".into(),
            tiers: vec![
                VerificationTier::Fast,
                VerificationTier::Standard,
                VerificationTier::Full,
            ],
            paths: vec!["ui/**".into()],
            affected_components: vec!["frontend".into()],
            cost_class: CostClass::Cheap,
            required: true,
            always_run: false,
            dependencies: vec![],
            estimated_duration_ms: None,
            command: None,
            integration_environment_spec: None,
            browser_test_spec: None,
        },
        VerificationCheck {
            check_id: "ui-unit".into(),
            name: "UI Unit Tests".into(),
            tiers: vec![
                VerificationTier::Fast,
                VerificationTier::Standard,
                VerificationTier::Full,
            ],
            paths: vec!["ui/**".into()],
            affected_components: vec!["frontend".into()],
            cost_class: CostClass::Cheap,
            required: true,
            always_run: false,
            dependencies: vec![],
            estimated_duration_ms: None,
            command: None,
            integration_environment_spec: None,
            browser_test_spec: None,
        },
        VerificationCheck {
            check_id: "browser-login".into(),
            name: "Browser Login Flow".into(),
            tiers: vec![VerificationTier::Standard, VerificationTier::Full],
            paths: vec!["ui/auth/**".into(), "tests/e2e/login/**".into()],
            affected_components: vec!["frontend".into(), "auth".into()],
            cost_class: CostClass::Expensive,
            required: true,
            always_run: false,
            dependencies: vec!["ui-unit".into(), "auth-unit".into()],
            estimated_duration_ms: None,
            command: None,
            integration_environment_spec: None,
            browser_test_spec: None,
        },
        VerificationCheck {
            check_id: "db-migration-test".into(),
            name: "DB Migration Test".into(),
            tiers: vec![VerificationTier::Standard, VerificationTier::Full],
            paths: vec!["migrations/**".into()],
            affected_components: vec!["database".into()],
            cost_class: CostClass::Medium,
            required: true,
            always_run: false,
            dependencies: vec![],
            estimated_duration_ms: None,
            command: None,
            integration_environment_spec: None,
            browser_test_spec: None,
        },
        VerificationCheck {
            check_id: "full-regression".into(),
            name: "Full Regression Suite".into(),
            tiers: vec![VerificationTier::Full],
            paths: vec!["**/*".into()],
            affected_components: vec![
                "core".into(),
                "auth".into(),
                "frontend".into(),
                "database".into(),
                "api".into(),
            ],
            cost_class: CostClass::Expensive,
            required: true,
            always_run: false,
            dependencies: vec![],
            estimated_duration_ms: None,
            command: None,
            integration_environment_spec: None,
            browser_test_spec: None,
        },
    ];
    pol.component_mappings = vec![
        ComponentMapping {
            pattern: "src/auth/**".into(),
            component: "auth".into(),
        },
        ComponentMapping {
            pattern: "tests/auth/**".into(),
            component: "auth_test".into(),
        },
        ComponentMapping {
            pattern: "ui/**".into(),
            component: "frontend".into(),
        },
        ComponentMapping {
            pattern: "migrations/**".into(),
            component: "database".into(),
        },
        ComponentMapping {
            pattern: "src/api/**".into(),
            component: "api".into(),
        },
    ];
    pol.broad_impact_paths = vec!["Cargo.toml".into(), "package.json".into()];
    pol.conservative_unknown_tier = VerificationTier::Standard;
    pol
}

async fn record_review_approval(ctx: &TestContext, wf_id: &str, ws_id: &str) -> Result<()> {
    let review = orbit::workflow::ReviewDecision {
        decision: orbit::workflow::ReviewDecisionStatus::Approve,
        summary: "Approved".into(),
        findings: vec![],
        requested_changes: vec![],
        suggested_additional_checks: vec![],
    };
    ctx.workflow_store
        .save_handoff_artifact(
            wf_id,
            None,
            orbit::workflow::HandoffType::Review,
            Some(ws_id),
            serde_json::to_value(&review)?,
        )
        .await?;
    Ok(())
}

fn sample_regression_policy(sel_pol: &SelectionPolicy) -> RegressionPolicy {
    let mut reg = RegressionPolicy::new("reg-policy-1", "Software Change Regression Policy");
    reg.feedback_tier = VerificationTier::Fast;
    reg.repair_tier = VerificationTier::Fast;
    reg.review_gate_tier = VerificationTier::Standard;
    reg.completion_tier = VerificationTier::Full;
    reg.selection_policy_id = Some(sel_pol.id.clone());
    reg.selection_policy_version = Some(sel_pol.version);
    reg.selection_policy_digest = Some(sel_pol.digest());
    reg
}

#[tokio::test]
async fn test_b6_01_auth_change_selection() -> Result<()> {
    let sel_pol = sample_selection_policy();
    let reg_pol = sample_regression_policy(&sel_pol);

    let changed = vec!["src/auth/token.rs".into()];
    let plan = select_verification(
        &sel_pol,
        Some(&reg_pol),
        "ws-auth-1",
        VerificationTier::Fast,
        &changed,
        &[],
        &[],
    )?;

    let selected_ids: Vec<_> = plan
        .selection
        .selected_checks
        .iter()
        .map(|s| &s.check_id)
        .collect();
    assert!(selected_ids.contains(&&"auth-unit".to_string()));
    assert!(selected_ids.contains(&&"format-check".to_string()));
    assert!(!selected_ids.contains(&&"ui-unit".to_string()));
    assert!(!selected_ids.contains(&&"db-migration-test".to_string()));

    let skipped_ids: Vec<_> = plan
        .selection
        .skipped_checks
        .iter()
        .map(|s| &s.check_id)
        .collect();
    assert!(skipped_ids.contains(&&"ui-unit".to_string()));
    assert!(skipped_ids.contains(&&"db-migration-test".to_string()));
    Ok(())
}

#[tokio::test]
async fn test_b6_02_frontend_change_selection() -> Result<()> {
    let sel_pol = sample_selection_policy();
    let reg_pol = sample_regression_policy(&sel_pol);

    let changed = vec!["ui/src/App.tsx".into()];
    let plan = select_verification(
        &sel_pol,
        Some(&reg_pol),
        "ws-ui-1",
        VerificationTier::Fast,
        &changed,
        &[],
        &[],
    )?;

    let selected_ids: Vec<_> = plan
        .selection
        .selected_checks
        .iter()
        .map(|s| &s.check_id)
        .collect();
    assert!(selected_ids.contains(&&"ui-lint".to_string()));
    assert!(selected_ids.contains(&&"ui-unit".to_string()));
    assert!(!selected_ids.contains(&&"auth-unit".to_string()));
    assert!(!selected_ids.contains(&&"db-migration-test".to_string()));
    Ok(())
}

#[tokio::test]
async fn test_b6_03_migration_change_escalation() -> Result<()> {
    let sel_pol = sample_selection_policy();
    let reg_pol = sample_regression_policy(&sel_pol);

    let changed = vec!["migrations/0002_add_users.sql".into()];
    let plan = select_verification(
        &sel_pol,
        Some(&reg_pol),
        "ws-mig-1",
        VerificationTier::Standard,
        &changed,
        &[],
        &[],
    )?;

    let selected_ids: Vec<_> = plan
        .selection
        .selected_checks
        .iter()
        .map(|s| &s.check_id)
        .collect();
    assert!(selected_ids.contains(&&"db-migration-test".to_string()));
    let sel_record = plan
        .selection
        .selected_checks
        .iter()
        .find(|s| s.check_id == "db-migration-test")
        .unwrap();
    assert!(matches!(
        sel_record.reason,
        SelectionReason::PolicyRequired | SelectionReason::PathMatch
    ));
    Ok(())
}

#[tokio::test]
async fn test_b6_04_build_change_escalation() -> Result<()> {
    let sel_pol = sample_selection_policy();
    let reg_pol = sample_regression_policy(&sel_pol);

    let changed = vec!["Cargo.toml".into()];
    let plan = select_verification(
        &sel_pol,
        Some(&reg_pol),
        "ws-build-1",
        VerificationTier::Standard,
        &changed,
        &[],
        &[],
    )?;

    // Broad impact Cargo.toml escalates Standard checks
    let selected_ids: Vec<_> = plan
        .selection
        .selected_checks
        .iter()
        .map(|s| &s.check_id)
        .collect();
    assert!(selected_ids.contains(&&"auth-integration".to_string()));
    assert!(selected_ids.contains(&&"browser-login".to_string()));
    Ok(())
}

#[tokio::test]
async fn test_b6_05_unknown_change_fallback() -> Result<()> {
    let sel_pol = sample_selection_policy();
    let reg_pol = sample_regression_policy(&sel_pol);

    let changed = vec!["random_unclassified_script.py".into()];
    let plan = select_verification(
        &sel_pol,
        Some(&reg_pol),
        "ws-unknown-1",
        VerificationTier::Standard,
        &changed,
        &[],
        &[],
    )?;

    // Unknown file triggers fallback to conservative tier checks
    assert!(
        plan.selection
            .selected_checks
            .iter()
            .any(|s| s.reason == SelectionReason::UnknownChangeFallback)
    );
    Ok(())
}

#[tokio::test]
async fn test_b6_06_previous_failure_rerun() -> Result<()> {
    let sel_pol = sample_selection_policy();
    let reg_pol = sample_regression_policy(&sel_pol);

    let changed = vec!["src/auth/token.rs".into()];
    let previous_failures = vec!["browser-login".to_string()];
    let plan = select_verification(
        &sel_pol,
        Some(&reg_pol),
        "ws-prev-1",
        VerificationTier::Standard,
        &changed,
        &previous_failures,
        &[],
    )?;

    let sel_record = plan
        .selection
        .selected_checks
        .iter()
        .find(|s| s.check_id == "browser-login")
        .unwrap();
    assert_eq!(sel_record.reason, SelectionReason::PreviousFailure);
    Ok(())
}

#[tokio::test]
async fn test_b6_07_reviewer_escalation() -> Result<()> {
    let sel_pol = sample_selection_policy();
    let reg_pol = sample_regression_policy(&sel_pol);

    let changed = vec!["src/auth/token.rs".into()];
    let reviewer_requests = vec!["browser-login".to_string()];
    let plan = select_verification(
        &sel_pol,
        Some(&reg_pol),
        "ws-rev-1",
        VerificationTier::Standard,
        &changed,
        &[],
        &reviewer_requests,
    )?;

    let sel_record = plan
        .selection
        .selected_checks
        .iter()
        .find(|s| s.check_id == "browser-login")
        .unwrap();
    assert_eq!(sel_record.reason, SelectionReason::ReviewerEscalation);
    Ok(())
}

#[tokio::test]
async fn test_b6_08_arbitrary_reviewer_command_rejected() -> Result<()> {
    let sel_pol = sample_selection_policy();
    let reg_pol = sample_regression_policy(&sel_pol);

    let changed = vec!["src/auth/token.rs".into()];
    let reviewer_requests = vec!["curl http://evil.com".to_string()];
    let res = select_verification(
        &sel_pol,
        Some(&reg_pol),
        "ws-rev-bad",
        VerificationTier::Standard,
        &changed,
        &[],
        &reviewer_requests,
    );

    assert!(res.is_err());
    let err_str = res.err().unwrap().to_string();
    assert!(err_str.contains("is not recognized in selection policy"));
    Ok(())
}

#[tokio::test]
#[ignore = "requires database URL"]
async fn test_b6_09_fast_not_full() -> Result<()> {
    let ctx = setup_regression_test().await?;
    let sel_pol = sample_selection_policy();
    let reg_pol = sample_regression_policy(&sel_pol);
    ctx.reg_store.insert_selection_policy(&sel_pol).await?;
    ctx.reg_store.insert_regression_policy(&reg_pol).await?;

    let ws = WorkspaceState::compute_from_parts("base-1", "head-1", Some("diff-b6-09"));
    let plan = select_verification(
        &sel_pol,
        Some(&reg_pol),
        &ws.state_id,
        VerificationTier::Fast,
        &["src/auth/token.rs".into()],
        &[],
        &[],
    )?;

    ctx.store.save_policy(&plan.policy).await?;
    ctx.reg_store.record_selection(&plan.selection).await?;

    // Create and pass a FAST verification run
    let env = sample_environment();
    let run = ctx
        .store
        .create_run_with_policy_and_tier(
            "att-1",
            &ws,
            &plan.plan,
            env.clone(),
            Some(&plan.policy),
            Some(VerificationTier::Fast),
            Some(&plan.selection),
            Some(&reg_pol),
        )
        .await?;

    for step in &plan.plan.steps {
        let srun = orbit::verification::VerificationStepRun {
            id: format!("step-{}", id()),
            verification_run_id: run.id.clone(),
            step_id: step.id.clone(),
            step_name: step.name.clone(),
            status: orbit::verification::VerificationStepStatus::Passed,
            required: step.required,
            exit_code: Some(0),
            started_at_ms: 1000,
            finished_at_ms: Some(1100),
            duration_ms: Some(100),
            stdout_preview: None,
            stdout_truncated: false,
            stdout_bytes: 0,
            stdout_artifact_id: None,
            stderr_preview: None,
            stderr_truncated: false,
            stderr_bytes: 0,
            stderr_artifact_id: None,
            artifacts: vec![],
            error_message: None,
        };
        ctx.store.record_step_run(&srun).await?;
    }
    ctx.store
        .finalize_run(&run.id, VerificationRunResult::Passed)
        .await?;

    // FAST qualifies FAST
    let qual_fast = ctx
        .store
        .check_workspace_qualification_with_tier(
            &ws.state_id,
            &plan.policy,
            VerificationTier::Fast,
            Some(&reg_pol),
            Some(&sel_pol),
            None,
        )
        .await?;
    assert!(qual_fast.is_some());

    // FAST DOES NOT QUALIFY FULL
    let qual_full = ctx
        .store
        .check_workspace_qualification_with_tier(
            &ws.state_id,
            &plan.policy,
            VerificationTier::Full,
            Some(&reg_pol),
            Some(&sel_pol),
            None,
        )
        .await?;
    assert!(qual_full.is_none(), "PASS(FAST) must never qualify FULL");

    // FAST DOES NOT QUALIFY STANDARD
    let qual_std = ctx
        .store
        .check_workspace_qualification_with_tier(
            &ws.state_id,
            &plan.policy,
            VerificationTier::Standard,
            Some(&reg_pol),
            Some(&sel_pol),
            None,
        )
        .await?;
    assert!(qual_std.is_none(), "PASS(FAST) must never qualify STANDARD");

    Ok(())
}

#[tokio::test]
#[ignore = "requires database URL"]
async fn test_b6_10_standard_not_full() -> Result<()> {
    let ctx = setup_regression_test().await?;
    let sel_pol = sample_selection_policy();
    let reg_pol = sample_regression_policy(&sel_pol);
    ctx.reg_store.insert_selection_policy(&sel_pol).await?;
    ctx.reg_store.insert_regression_policy(&reg_pol).await?;

    let ws = WorkspaceState::compute_from_parts("base-2", "head-2", Some("diff-b6-10"));
    let plan = select_verification(
        &sel_pol,
        Some(&reg_pol),
        &ws.state_id,
        VerificationTier::Standard,
        &["src/auth/token.rs".into()],
        &[],
        &[],
    )?;

    ctx.store.save_policy(&plan.policy).await?;
    ctx.reg_store.record_selection(&plan.selection).await?;

    // Create and pass a STANDARD verification run
    let env = sample_environment();
    let run = ctx
        .store
        .create_run_with_policy_and_tier(
            "att-2",
            &ws,
            &plan.plan,
            env.clone(),
            Some(&plan.policy),
            Some(VerificationTier::Standard),
            Some(&plan.selection),
            Some(&reg_pol),
        )
        .await?;

    for step in &plan.plan.steps {
        let srun = orbit::verification::VerificationStepRun {
            id: format!("step-{}", id()),
            verification_run_id: run.id.clone(),
            step_id: step.id.clone(),
            step_name: step.name.clone(),
            status: orbit::verification::VerificationStepStatus::Passed,
            required: step.required,
            exit_code: Some(0),
            started_at_ms: 1000,
            finished_at_ms: Some(1100),
            duration_ms: Some(100),
            stdout_preview: None,
            stdout_truncated: false,
            stdout_bytes: 0,
            stdout_artifact_id: None,
            stderr_preview: None,
            stderr_truncated: false,
            stderr_bytes: 0,
            stderr_artifact_id: None,
            artifacts: vec![],
            error_message: None,
        };
        ctx.store.record_step_run(&srun).await?;
    }
    ctx.store
        .finalize_run(&run.id, VerificationRunResult::Passed)
        .await?;

    // STANDARD qualifies STANDARD
    let qual_std = ctx
        .store
        .check_workspace_qualification_with_tier(
            &ws.state_id,
            &plan.policy,
            VerificationTier::Standard,
            Some(&reg_pol),
            Some(&sel_pol),
            None,
        )
        .await?;
    assert!(qual_std.is_some());

    // STANDARD DOES NOT QUALIFY FULL
    let qual_full = ctx
        .store
        .check_workspace_qualification_with_tier(
            &ws.state_id,
            &plan.policy,
            VerificationTier::Full,
            Some(&reg_pol),
            Some(&sel_pol),
            None,
        )
        .await?;
    assert!(
        qual_full.is_none(),
        "PASS(STANDARD) must never qualify FULL"
    );

    Ok(())
}

#[tokio::test]
#[ignore = "requires database URL"]
async fn test_b6_11_full_completion_gate() -> Result<()> {
    let ctx = setup_regression_test().await?;
    let sel_pol = sample_selection_policy();
    let reg_pol = sample_regression_policy(&sel_pol);
    ctx.reg_store.insert_selection_policy(&sel_pol).await?;
    ctx.reg_store.insert_regression_policy(&reg_pol).await?;

    let ws = WorkspaceState::compute_from_parts("base-3", "head-3", Some("diff-b6-11"));
    let plan = select_verification(
        &sel_pol,
        Some(&reg_pol),
        &ws.state_id,
        VerificationTier::Full,
        &["src/auth/token.rs".into()],
        &[],
        &[],
    )?;

    ctx.store.save_policy(&plan.policy).await?;
    ctx.reg_store.record_selection(&plan.selection).await?;

    let task_id = format!("task-{}", id());
    let attempt_id = format!("att-{}", id());
    let mut wf = ctx
        .workflow_store
        .create_workflow_run_with_regression_policy(
            &task_id,
            &attempt_id,
            3,
            Some(&plan.policy),
            Some(&reg_pol),
        )
        .await?;

    wf = ctx
        .workflow_store
        .transition_workflow_stage(&wf.id, WorkflowStage::Planning, None, None, None)
        .await?;
    wf = ctx
        .workflow_store
        .transition_workflow_stage(&wf.id, WorkflowStage::Implementing, None, None, None)
        .await?;
    wf = ctx
        .workflow_store
        .transition_workflow_stage(
            &wf.id,
            WorkflowStage::Verifying,
            Some(&ws.state_id),
            None,
            None,
        )
        .await?;

    // Create and pass a FULL verification run
    let env = sample_environment();
    let run = ctx
        .store
        .create_run_with_policy_and_tier(
            &attempt_id,
            &ws,
            &plan.plan,
            env.clone(),
            Some(&plan.policy),
            Some(VerificationTier::Full),
            Some(&plan.selection),
            Some(&reg_pol),
        )
        .await?;

    for step in &plan.plan.steps {
        let srun = orbit::verification::VerificationStepRun {
            id: format!("step-{}", id()),
            verification_run_id: run.id.clone(),
            step_id: step.id.clone(),
            step_name: step.name.clone(),
            status: orbit::verification::VerificationStepStatus::Passed,
            required: step.required,
            exit_code: Some(0),
            started_at_ms: 1000,
            finished_at_ms: Some(1100),
            duration_ms: Some(100),
            stdout_preview: None,
            stdout_truncated: false,
            stdout_bytes: 0,
            stdout_artifact_id: None,
            stderr_preview: None,
            stderr_truncated: false,
            stderr_bytes: 0,
            stderr_artifact_id: None,
            artifacts: vec![],
            error_message: None,
        };
        ctx.store.record_step_run(&srun).await?;
    }
    ctx.store
        .finalize_run(&run.id, VerificationRunResult::Passed)
        .await?;
    record_review_approval(&ctx, &wf.id, &ws.state_id).await?;

    // Check completion invariant succeeds because FULL passed!
    ctx.workflow_store
        .check_completion_invariant(&wf.id)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires database URL"]
async fn test_b6_12_full_failure_blocks_completion() -> Result<()> {
    let ctx = setup_regression_test().await?;
    let sel_pol = sample_selection_policy();
    let reg_pol = sample_regression_policy(&sel_pol);
    ctx.reg_store.insert_selection_policy(&sel_pol).await?;
    ctx.reg_store.insert_regression_policy(&reg_pol).await?;

    let ws = WorkspaceState::compute_from_parts("base-4", "head-4", Some("diff-b6-12"));
    let plan = select_verification(
        &sel_pol,
        Some(&reg_pol),
        &ws.state_id,
        VerificationTier::Fast,
        &["src/auth/token.rs".into()],
        &[],
        &[],
    )?;

    ctx.store.save_policy(&plan.policy).await?;
    ctx.reg_store.record_selection(&plan.selection).await?;

    let task_id = format!("task-{}", id());
    let attempt_id = format!("att-{}", id());
    let mut wf = ctx
        .workflow_store
        .create_workflow_run_with_regression_policy(
            &task_id,
            &attempt_id,
            3,
            Some(&plan.policy),
            Some(&reg_pol),
        )
        .await?;

    wf = ctx
        .workflow_store
        .transition_workflow_stage(&wf.id, WorkflowStage::Planning, None, None, None)
        .await?;
    wf = ctx
        .workflow_store
        .transition_workflow_stage(&wf.id, WorkflowStage::Implementing, None, None, None)
        .await?;
    wf = ctx
        .workflow_store
        .transition_workflow_stage(
            &wf.id,
            WorkflowStage::Verifying,
            Some(&ws.state_id),
            None,
            None,
        )
        .await?;

    // Create only a FAST run (which passes)
    let env = sample_environment();
    let run = ctx
        .store
        .create_run_with_policy_and_tier(
            &attempt_id,
            &ws,
            &plan.plan,
            env.clone(),
            Some(&plan.policy),
            Some(VerificationTier::Fast),
            Some(&plan.selection),
            Some(&reg_pol),
        )
        .await?;

    for step in &plan.plan.steps {
        let srun = orbit::verification::VerificationStepRun {
            id: format!("step-{}", id()),
            verification_run_id: run.id.clone(),
            step_id: step.id.clone(),
            step_name: step.name.clone(),
            status: orbit::verification::VerificationStepStatus::Passed,
            required: step.required,
            exit_code: Some(0),
            started_at_ms: 1000,
            finished_at_ms: Some(1100),
            duration_ms: Some(100),
            stdout_preview: None,
            stdout_truncated: false,
            stdout_bytes: 0,
            stdout_artifact_id: None,
            stderr_preview: None,
            stderr_truncated: false,
            stderr_bytes: 0,
            stderr_artifact_id: None,
            artifacts: vec![],
            error_message: None,
        };
        ctx.store.record_step_run(&srun).await?;
    }
    ctx.store
        .finalize_run(&run.id, VerificationRunResult::Passed)
        .await?;
    record_review_approval(&ctx, &wf.id, &ws.state_id).await?;

    // Invariant check MUST fail because FULL regression has not qualified!
    let res = ctx.workflow_store.check_completion_invariant(&wf.id).await;
    assert!(
        res.is_err(),
        "completion must be blocked when FULL regression is absent"
    );
    let err_str = res.err().unwrap().to_string();
    assert!(err_str.contains("has not qualified required completion tier Full"));
    Ok(())
}

#[tokio::test]
#[ignore = "requires database URL"]
async fn test_b6_13_selection_policy_mutation_invalidation() -> Result<()> {
    let ctx = setup_regression_test().await?;
    let sel_pol = sample_selection_policy();
    let reg_pol = sample_regression_policy(&sel_pol);
    ctx.reg_store.insert_selection_policy(&sel_pol).await?;
    ctx.reg_store.insert_regression_policy(&reg_pol).await?;

    let ws = WorkspaceState::compute_from_parts("base-5", "head-5", Some("diff-b6-13"));
    let plan = select_verification(
        &sel_pol,
        Some(&reg_pol),
        &ws.state_id,
        VerificationTier::Full,
        &["src/auth/token.rs".into()],
        &[],
        &[],
    )?;

    ctx.store.save_policy(&plan.policy).await?;
    ctx.reg_store.record_selection(&plan.selection).await?;

    let env = sample_environment();
    let run = ctx
        .store
        .create_run_with_policy_and_tier(
            "att-sp-mut",
            &ws,
            &plan.plan,
            env.clone(),
            Some(&plan.policy),
            Some(VerificationTier::Full),
            Some(&plan.selection),
            Some(&reg_pol),
        )
        .await?;

    for step in &plan.plan.steps {
        let srun = orbit::verification::VerificationStepRun {
            id: format!("step-{}", id()),
            verification_run_id: run.id.clone(),
            step_id: step.id.clone(),
            step_name: step.name.clone(),
            status: orbit::verification::VerificationStepStatus::Passed,
            required: step.required,
            exit_code: Some(0),
            started_at_ms: 1000,
            finished_at_ms: Some(1100),
            duration_ms: Some(100),
            stdout_preview: None,
            stdout_truncated: false,
            stdout_bytes: 0,
            stdout_artifact_id: None,
            stderr_preview: None,
            stderr_truncated: false,
            stderr_bytes: 0,
            stderr_artifact_id: None,
            artifacts: vec![],
            error_message: None,
        };
        ctx.store.record_step_run(&srun).await?;
    }
    ctx.store
        .finalize_run(&run.id, VerificationRunResult::Passed)
        .await?;

    // Valid with original policy
    let qual = ctx
        .store
        .check_workspace_qualification_with_tier(
            &ws.state_id,
            &plan.policy,
            VerificationTier::Full,
            Some(&reg_pol),
            Some(&sel_pol),
            None,
        )
        .await?;
    assert!(qual.is_some());

    // Mutated selection policy (digest changed)
    let mut mutated_sel_pol = sel_pol.clone();
    mutated_sel_pol.conservative_unknown_tier = VerificationTier::Full;
    assert_ne!(sel_pol.digest(), mutated_sel_pol.digest());

    let qual_mutated = ctx
        .store
        .check_workspace_qualification_with_tier(
            &ws.state_id,
            &plan.policy,
            VerificationTier::Full,
            Some(&reg_pol),
            Some(&mutated_sel_pol),
            None,
        )
        .await?;
    assert!(
        qual_mutated.is_none(),
        "selection policy mutation must invalidate qualification"
    );

    Ok(())
}

#[tokio::test]
#[ignore = "requires database URL"]
async fn test_b6_14_regression_policy_mutation_invalidation() -> Result<()> {
    let ctx = setup_regression_test().await?;
    let sel_pol = sample_selection_policy();
    let reg_pol = sample_regression_policy(&sel_pol);
    ctx.reg_store.insert_selection_policy(&sel_pol).await?;
    ctx.reg_store.insert_regression_policy(&reg_pol).await?;

    let ws = WorkspaceState::compute_from_parts("base-6", "head-6", Some("diff-b6-14"));
    let plan = select_verification(
        &sel_pol,
        Some(&reg_pol),
        &ws.state_id,
        VerificationTier::Full,
        &["src/auth/token.rs".into()],
        &[],
        &[],
    )?;

    ctx.store.save_policy(&plan.policy).await?;
    ctx.reg_store.record_selection(&plan.selection).await?;

    let env = sample_environment();
    let run = ctx
        .store
        .create_run_with_policy_and_tier(
            "att-rp-mut",
            &ws,
            &plan.plan,
            env.clone(),
            Some(&plan.policy),
            Some(VerificationTier::Full),
            Some(&plan.selection),
            Some(&reg_pol),
        )
        .await?;

    for step in &plan.plan.steps {
        let srun = orbit::verification::VerificationStepRun {
            id: format!("step-{}", id()),
            verification_run_id: run.id.clone(),
            step_id: step.id.clone(),
            step_name: step.name.clone(),
            status: orbit::verification::VerificationStepStatus::Passed,
            required: step.required,
            exit_code: Some(0),
            started_at_ms: 1000,
            finished_at_ms: Some(1100),
            duration_ms: Some(100),
            stdout_preview: None,
            stdout_truncated: false,
            stdout_bytes: 0,
            stdout_artifact_id: None,
            stderr_preview: None,
            stderr_truncated: false,
            stderr_bytes: 0,
            stderr_artifact_id: None,
            artifacts: vec![],
            error_message: None,
        };
        ctx.store.record_step_run(&srun).await?;
    }
    ctx.store
        .finalize_run(&run.id, VerificationRunResult::Passed)
        .await?;

    // Valid with original policy
    let qual = ctx
        .store
        .check_workspace_qualification_with_tier(
            &ws.state_id,
            &plan.policy,
            VerificationTier::Full,
            Some(&reg_pol),
            Some(&sel_pol),
            None,
        )
        .await?;
    assert!(qual.is_some());

    // Mutated regression policy (e.g. review_gate_tier changed)
    let mut mutated_reg_pol = reg_pol.clone();
    mutated_reg_pol.review_gate_tier = VerificationTier::Full;
    assert_ne!(reg_pol.digest(), mutated_reg_pol.digest());

    let qual_mutated = ctx
        .store
        .check_workspace_qualification_with_tier(
            &ws.state_id,
            &plan.policy,
            VerificationTier::Full,
            Some(&mutated_reg_pol),
            Some(&sel_pol),
            None,
        )
        .await?;
    assert!(
        qual_mutated.is_none(),
        "regression policy mutation must invalidate qualification"
    );

    Ok(())
}

#[tokio::test]
#[ignore = "requires database URL"]
async fn test_b6_15_workspace_mutation_invalidation() -> Result<()> {
    let ctx = setup_regression_test().await?;
    let sel_pol = sample_selection_policy();
    let reg_pol = sample_regression_policy(&sel_pol);

    let ws1 = WorkspaceState::compute_from_parts("base-7", "head-7a", Some("diff-b6-15a"));
    let ws2 = WorkspaceState::compute_from_parts("base-7", "head-7b", Some("diff-b6-15b"));
    assert_ne!(ws1.state_id, ws2.state_id);

    let plan = select_verification(
        &sel_pol,
        Some(&reg_pol),
        &ws1.state_id,
        VerificationTier::Full,
        &["src/auth/token.rs".into()],
        &[],
        &[],
    )?;
    ctx.store.save_policy(&plan.policy).await?;
    ctx.reg_store.record_selection(&plan.selection).await?;

    let env = sample_environment();
    let run = ctx
        .store
        .create_run_with_policy_and_tier(
            "att-ws-mut",
            &ws1,
            &plan.plan,
            env.clone(),
            Some(&plan.policy),
            Some(VerificationTier::Full),
            Some(&plan.selection),
            Some(&reg_pol),
        )
        .await?;

    for step in &plan.plan.steps {
        let srun = orbit::verification::VerificationStepRun {
            id: format!("step-{}", id()),
            verification_run_id: run.id.clone(),
            step_id: step.id.clone(),
            step_name: step.name.clone(),
            status: orbit::verification::VerificationStepStatus::Passed,
            required: step.required,
            exit_code: Some(0),
            started_at_ms: 1000,
            finished_at_ms: Some(1100),
            duration_ms: Some(100),
            stdout_preview: None,
            stdout_truncated: false,
            stdout_bytes: 0,
            stdout_artifact_id: None,
            stderr_preview: None,
            stderr_truncated: false,
            stderr_bytes: 0,
            stderr_artifact_id: None,
            artifacts: vec![],
            error_message: None,
        };
        ctx.store.record_step_run(&srun).await?;
    }
    ctx.store
        .finalize_run(&run.id, VerificationRunResult::Passed)
        .await?;

    // ws1 qualifies
    let q1 = ctx
        .store
        .check_workspace_qualification_with_tier(
            &ws1.state_id,
            &plan.policy,
            VerificationTier::Full,
            Some(&reg_pol),
            Some(&sel_pol),
            None,
        )
        .await?;
    assert!(q1.is_some());

    // ws2 does NOT qualify
    let q2 = ctx
        .store
        .check_workspace_qualification_with_tier(
            &ws2.state_id,
            &plan.policy,
            VerificationTier::Full,
            Some(&reg_pol),
            Some(&sel_pol),
            None,
        )
        .await?;
    assert!(
        q2.is_none(),
        "workspace mutation must invalidate qualification"
    );

    Ok(())
}

#[tokio::test]
async fn test_b6_16_check_dependency_expansion() -> Result<()> {
    let sel_pol = sample_selection_policy();
    let reg_pol = sample_regression_policy(&sel_pol);

    // changed file tests/auth/** triggers auth-integration which depends on auth-unit
    let changed = vec!["tests/auth/login_test.rs".into()];
    let plan = select_verification(
        &sel_pol,
        Some(&reg_pol),
        "ws-dep-1",
        VerificationTier::Standard,
        &changed,
        &[],
        &[],
    )?;

    let selected_ids: Vec<_> = plan
        .selection
        .selected_checks
        .iter()
        .map(|s| &s.check_id)
        .collect();
    assert!(selected_ids.contains(&&"auth-integration".to_string()));
    assert!(
        selected_ids.contains(&&"auth-unit".to_string()),
        "dependency auth-unit must be expanded"
    );

    let dep_record = plan
        .selection
        .selected_checks
        .iter()
        .find(|s| s.check_id == "auth-unit")
        .unwrap();
    assert_eq!(dep_record.reason, SelectionReason::CheckDependency);
    assert!(
        dep_record
            .reason_detail
            .contains("prerequisite dependency of check 'auth-integration'")
    );
    Ok(())
}

#[tokio::test]
async fn test_b6_17_skip_reason_evidence() -> Result<()> {
    let sel_pol = sample_selection_policy();
    let reg_pol = sample_regression_policy(&sel_pol);

    let changed = vec!["src/auth/token.rs".into()];
    let plan = select_verification(
        &sel_pol,
        Some(&reg_pol),
        "ws-skip-1",
        VerificationTier::Fast,
        &changed,
        &[],
        &[],
    )?;

    // Inspect skipped checks
    assert!(!plan.selection.skipped_checks.is_empty());
    for skipped in &plan.selection.skipped_checks {
        assert!(!skipped.check_id.is_empty());
        assert!(!skipped.reason_detail.is_empty());
        assert!(matches!(
            skipped.reason,
            SelectionReason::TierExcluded | SelectionReason::NoAffectedComponent
        ));
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires database URL"]
async fn test_b6_18_restart_durability() -> Result<()> {
    let ctx = setup_regression_test().await?;
    let sel_pol = sample_selection_policy();
    let reg_pol = sample_regression_policy(&sel_pol);

    ctx.reg_store.insert_selection_policy(&sel_pol).await?;
    ctx.reg_store.insert_regression_policy(&reg_pol).await?;

    let plan = select_verification(
        &sel_pol,
        Some(&reg_pol),
        "ws-durable-1",
        VerificationTier::Standard,
        &["src/auth/token.rs".into()],
        &[],
        &[],
    )?;
    ctx.reg_store.record_selection(&plan.selection).await?;

    // Create a fresh store instance pointing to the same database
    let fresh_engine =
        Engine::connect(&ctx.url, tempfile::tempdir()?.path().join("artifacts"), 3).await?;
    let fresh_reg_store = RegressionStore::new(fresh_engine.pool.clone());

    // Verify selection policy reloaded
    let reloaded_sp = fresh_reg_store
        .get_selection_policy(&sel_pol.id, sel_pol.version)
        .await?
        .unwrap();
    assert_eq!(sel_pol.digest(), reloaded_sp.digest());

    // Verify regression policy reloaded
    let reloaded_rp = fresh_reg_store
        .get_regression_policy(&reg_pol.id, reg_pol.version)
        .await?
        .unwrap();
    assert_eq!(reg_pol.digest(), reloaded_rp.digest());

    // Verify selection reloaded
    let reloaded_sel = fresh_reg_store
        .get_selection(&plan.selection.id)
        .await?
        .unwrap();
    assert_eq!(plan.selection.digest, reloaded_sel.digest);
    assert_eq!(plan.selection.requested_tier, reloaded_sel.requested_tier);
    assert_eq!(
        plan.selection.selected_checks.len(),
        reloaded_sel.selected_checks.len()
    );

    Ok(())
}

#[tokio::test]
#[ignore = "requires database URL"]
async fn test_b6_19_b3_workflow_integration() -> Result<()> {
    let ctx = setup_regression_test().await?;
    let sel_pol = sample_selection_policy();
    let reg_pol = sample_regression_policy(&sel_pol);
    ctx.reg_store.insert_selection_policy(&sel_pol).await?;
    ctx.reg_store.insert_regression_policy(&reg_pol).await?;

    let ws = WorkspaceState::compute_from_parts("base-8", "head-8", Some("diff-b6-19"));
    let plan = select_verification(
        &sel_pol,
        Some(&reg_pol),
        &ws.state_id,
        VerificationTier::Full,
        &["src/auth/token.rs".into()],
        &[],
        &[],
    )?;
    ctx.store.save_policy(&plan.policy).await?;
    ctx.reg_store.record_selection(&plan.selection).await?;

    let task_id = format!("task-{}", id());
    let attempt_id = format!("att-{}", id());
    let mut wf = ctx
        .workflow_store
        .create_workflow_run_with_regression_policy(
            &task_id,
            &attempt_id,
            3,
            Some(&plan.policy),
            Some(&reg_pol),
        )
        .await?;

    wf = ctx
        .workflow_store
        .transition_workflow_stage(&wf.id, WorkflowStage::Planning, None, None, None)
        .await?;
    wf = ctx
        .workflow_store
        .transition_workflow_stage(&wf.id, WorkflowStage::Implementing, None, None, None)
        .await?;
    wf = ctx
        .workflow_store
        .transition_workflow_stage(
            &wf.id,
            WorkflowStage::Verifying,
            Some(&ws.state_id),
            None,
            None,
        )
        .await?;

    // Create and pass FULL tier verification
    let env = sample_environment();
    let run = ctx
        .store
        .create_run_with_policy_and_tier(
            &attempt_id,
            &ws,
            &plan.plan,
            env,
            Some(&plan.policy),
            Some(VerificationTier::Full),
            Some(&plan.selection),
            Some(&reg_pol),
        )
        .await?;

    for step in &plan.plan.steps {
        let srun = orbit::verification::VerificationStepRun {
            id: format!("step-{}", id()),
            verification_run_id: run.id.clone(),
            step_id: step.id.clone(),
            step_name: step.name.clone(),
            status: orbit::verification::VerificationStepStatus::Passed,
            required: step.required,
            exit_code: Some(0),
            started_at_ms: 1000,
            finished_at_ms: Some(1100),
            duration_ms: Some(100),
            stdout_preview: None,
            stdout_truncated: false,
            stdout_bytes: 0,
            stdout_artifact_id: None,
            stderr_preview: None,
            stderr_truncated: false,
            stderr_bytes: 0,
            stderr_artifact_id: None,
            artifacts: vec![],
            error_message: None,
        };
        ctx.store.record_step_run(&srun).await?;
    }
    ctx.store
        .finalize_run(&run.id, VerificationRunResult::Passed)
        .await?;

    // Transition to Reviewing
    wf = ctx
        .workflow_store
        .transition_workflow_stage(
            &wf.id,
            WorkflowStage::Reviewing,
            Some(&ws.state_id),
            None,
            None,
        )
        .await?;
    assert_eq!(wf.status, WorkflowStage::Reviewing);
    record_review_approval(&ctx, &wf.id, &ws.state_id).await?;

    // Transition to Regression
    wf = ctx
        .workflow_store
        .transition_workflow_stage(
            &wf.id,
            WorkflowStage::Regression,
            Some(&ws.state_id),
            None,
            None,
        )
        .await?;
    assert_eq!(wf.status, WorkflowStage::Regression);

    // Transition to Completed (succeeds because FULL verified)
    ctx.workflow_store
        .check_completion_invariant(&wf.id)
        .await?;
    wf = ctx
        .workflow_store
        .transition_workflow_stage(
            &wf.id,
            WorkflowStage::Completed,
            Some(&ws.state_id),
            None,
            None,
        )
        .await?;
    assert_eq!(wf.status, WorkflowStage::Completed);

    Ok(())
}

#[tokio::test]
#[ignore = "requires database URL"]
async fn test_b6_20_b4_integration() -> Result<()> {
    let ctx = setup_regression_test().await?;
    let mut sel_pol = sample_selection_policy();

    // Add check with integration environment spec
    let env_spec = orbit::integration_environment::IntegrationEnvironmentSpec {
        id: "env-svc-test".into(),
        version: 1,
        services: vec![orbit::integration_environment::ManagedServiceSpec {
            id: "echo-svc".into(),
            kind: orbit::integration_environment::ServiceKind::Process,
            image: None,
            command: vec!["sleep".into(), "60".into()],
            args: vec![],
            env: BTreeMap::new(),
            mounts: vec![],
            internal_port: None,
            readiness: None,
            timeout_seconds: 60,
            dependencies: vec![],
        }],
        network_policy: VerificationNetworkPolicy::Isolated,
        setup_steps: vec![],
    };

    sel_pol.checks.push(VerificationCheck {
        check_id: "svc-check".into(),
        name: "Service Check".into(),
        tiers: vec![VerificationTier::Standard, VerificationTier::Full],
        paths: vec!["src/api/**".into()],
        affected_components: vec!["api".into()],
        cost_class: CostClass::Medium,
        required: true,
        always_run: true,
        dependencies: vec![],
        estimated_duration_ms: None,
        command: None,
        integration_environment_spec: Some(env_spec),
        browser_test_spec: None,
    });

    let reg_pol = sample_regression_policy(&sel_pol);
    ctx.reg_store.insert_selection_policy(&sel_pol).await?;
    ctx.reg_store.insert_regression_policy(&reg_pol).await?;

    let plan = select_verification(
        &sel_pol,
        Some(&reg_pol),
        "ws-b4-int",
        VerificationTier::Standard,
        &["src/api/handler.rs".into()],
        &[],
        &[],
    )?;

    assert!(plan.plan.steps.iter().any(|s| s.id == "svc-check"));
    assert!(plan.policy.integration_environment_spec.is_some());
    Ok(())
}

#[tokio::test]
#[ignore = "requires database URL"]
async fn test_b6_21_b5_integration() -> Result<()> {
    let _ctx = setup_regression_test().await?;
    let mut sel_pol = sample_selection_policy();

    let btest = orbit::browser_verification::BrowserTestSpec {
        id: "login-browser-test".into(),
        name: "Login UI Verification".into(),
        entrypoint: "tests/browser/login.spec.ts".into(),
        command: None,
        timeout_seconds: 10,
        required: true,
    };

    sel_pol.checks.push(VerificationCheck {
        check_id: "browser-ui-check".into(),
        name: "Browser UI Check".into(),
        tiers: vec![VerificationTier::Standard, VerificationTier::Full],
        paths: vec!["ui/**".into()],
        affected_components: vec!["frontend".into()],
        cost_class: CostClass::Expensive,
        required: true,
        always_run: true,
        dependencies: vec![],
        estimated_duration_ms: None,
        command: None,
        integration_environment_spec: None,
        browser_test_spec: Some(btest),
    });

    let reg_pol = sample_regression_policy(&sel_pol);
    let plan = select_verification(
        &sel_pol,
        Some(&reg_pol),
        "ws-b5-int",
        VerificationTier::Standard,
        &["ui/src/App.tsx".into()],
        &[],
        &[],
    )?;

    assert!(plan.policy.browser_verification_spec.is_some());
    let b_spec = plan.policy.browser_verification_spec.unwrap();
    assert_eq!(b_spec.tests.len(), 1);
    assert_eq!(b_spec.tests[0].id, "login-browser-test");
    Ok(())
}
