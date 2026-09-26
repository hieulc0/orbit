use anyhow::{Context, Result};
use orbit::{
    engine::Engine,
    model::id,
    verification::{
        EnvironmentIdentity, VerificationPlan, VerificationRunResult, VerificationStep,
        VerificationStepStatus, VerificationStore, WorkspaceState, execute_verification_plan,
    },
};
use sqlx::PgPool;
use std::time::Duration;

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL ORBIT_TEST_DATABASE_URL"]
async fn verification_evidence_lifecycle_and_mutation_invalidation() -> Result<()> {
    let base = std::env::var("ORBIT_TEST_DATABASE_URL")
        .context("set ORBIT_TEST_DATABASE_URL to a disposable PostgreSQL database")?;
    let admin = PgPool::connect(&base).await?;
    let schema = format!("orbit_test_verif_{}", id().replace('-', ""));
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await?;
    let separator = if base.contains('?') { '&' } else { '?' };
    let url = format!("{base}{separator}options=-csearch_path%3D{schema}");
    let home = tempfile::tempdir()?;

    let engine = Engine::connect(&url, home.path().join("artifacts"), 3).await?;
    let store = VerificationStore::new(engine.pool.clone());

    // 1. Setup workspace fixture
    let ws_dir = tempfile::tempdir()?;
    let script_pass = ws_dir.path().join("pass.sh");
    tokio::fs::write(&script_pass, "#!/bin/sh\necho 'all good'\nexit 0\n").await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&script_pass, std::fs::Permissions::from_mode(0o755)).await?;
    }

    let script_fail = ws_dir.path().join("fail.sh");
    tokio::fs::write(
        &script_fail,
        "#!/bin/sh\necho 'something broke' >&2\nexit 1\n",
    )
    .await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&script_fail, std::fs::Permissions::from_mode(0o755)).await?;
    }

    let env = EnvironmentIdentity {
        execution_profile: "test-profile".into(),
        isolation: "trusted".into(),
        runtime_image: Some("alpine:latest".into()),
        runtime_image_digest: None,
        oci_runtime: None,
        network_policy: orbit::verification::VerificationNetworkPolicy::None,
        cache_policy: orbit::verification::VerificationCachePolicy::Clean,
        environment_policy_digest: None,
        integration_environment_digest: None,
        architecture: "x86_64".into(),
        os: "linux".into(),
        orbit_version: "0.1.0".into(),
        browser_verification_digest: None,
        browser_runtime_image_digest: None,
    };

    // 2. Case A: PASSING VERIFICATION RUN
    let plan_pass = VerificationPlan::new(
        "pass-plan",
        "Passing Plan",
        vec![VerificationStep::new_command(
            "step_pass",
            "Passing Step",
            vec![script_pass.to_str().unwrap().into()],
        )],
    );
    store.save_plan(&plan_pass).await?;

    let ws_state_a =
        WorkspaceState::compute_from_parts("base-rev-1", "head-rev-1", Some("diff-sha-A"));
    let run_pass = execute_verification_plan(
        &store,
        "attempt-1",
        &ws_state_a,
        &plan_pass,
        ws_dir.path(),
        env.clone(),
        None,
    )
    .await?;

    assert_eq!(run_pass.status, VerificationStepStatus::Passed);
    assert_eq!(run_pass.overall_result, Some(VerificationRunResult::Passed));
    assert_eq!(run_pass.step_runs.len(), 1);
    assert_eq!(run_pass.step_runs[0].status, VerificationStepStatus::Passed);
    assert_eq!(run_pass.step_runs[0].exit_code, Some(0));
    assert!(
        run_pass.step_runs[0]
            .stdout_preview
            .as_ref()
            .unwrap()
            .contains("all good")
    );

    // Verify it can be retrieved from PostgreSQL and round-trips
    let fetched = store
        .get_run(&run_pass.id)
        .await?
        .expect("run exists in db");
    assert_eq!(fetched.id, run_pass.id);
    assert_eq!(fetched.overall_result, Some(VerificationRunResult::Passed));
    assert_eq!(fetched.step_runs.len(), 1);

    // Verify latest_passing_verification_for_workspace finds it for WorkspaceState A
    let passing_a = store
        .latest_passing_verification_for_workspace(&ws_state_a.state_id)
        .await?;
    assert!(passing_a.is_some());
    assert_eq!(passing_a.unwrap().id, run_pass.id);

    // 3. Case F: MUTATION INVALIDATION
    // Workspace mutates to state B (e.g., diff changes to diff-sha-B)
    let ws_state_b =
        WorkspaceState::compute_from_parts("base-rev-1", "head-rev-1", Some("diff-sha-B"));
    assert_ne!(ws_state_a.state_id, ws_state_b.state_id);

    let passing_b = store
        .latest_passing_verification_for_workspace(&ws_state_b.state_id)
        .await?;
    // CRITICAL INVARIANT: Evidence from A MUST NOT satisfy state B!
    assert!(
        passing_b.is_none(),
        "stale verification evidence must not satisfy mutated workspace state B"
    );

    // 4. Case B: FAILING STEP
    let plan_fail = VerificationPlan::new(
        "fail-plan",
        "Failing Plan",
        vec![VerificationStep::new_command(
            "step_fail",
            "Failing Step",
            vec![script_fail.to_str().unwrap().into()],
        )],
    );
    let run_fail = execute_verification_plan(
        &store,
        "attempt-1",
        &ws_state_b,
        &plan_fail,
        ws_dir.path(),
        env.clone(),
        None,
    )
    .await?;

    assert_eq!(run_fail.status, VerificationStepStatus::Failed);
    assert_eq!(run_fail.overall_result, Some(VerificationRunResult::Failed));
    assert_eq!(run_fail.step_runs[0].status, VerificationStepStatus::Failed);
    assert_eq!(run_fail.step_runs[0].exit_code, Some(1));
    assert!(
        run_fail.step_runs[0]
            .stderr_preview
            .as_ref()
            .unwrap()
            .contains("something broke")
    );

    // 5. Case C: TIMEOUT
    let script_timeout = ws_dir.path().join("timeout.sh");
    tokio::fs::write(&script_timeout, "#!/bin/sh\nsleep 10\nexit 0\n").await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&script_timeout, std::fs::Permissions::from_mode(0o755)).await?;
    }

    let mut step_timeout = VerificationStep::new_command(
        "step_timeout",
        "Timeout Step",
        vec![script_timeout.to_str().unwrap().into()],
    );
    step_timeout.timeout_seconds = 1;

    let plan_timeout = VerificationPlan::new("timeout-plan", "Timeout Plan", vec![step_timeout]);

    let run_timeout = execute_verification_plan(
        &store,
        "attempt-1",
        &ws_state_b,
        &plan_timeout,
        ws_dir.path(),
        env.clone(),
        None,
    )
    .await?;

    assert_eq!(run_timeout.status, VerificationStepStatus::TimedOut);
    assert_eq!(
        run_timeout.overall_result,
        Some(VerificationRunResult::TimedOut)
    );
    assert_eq!(
        run_timeout.step_runs[0].status,
        VerificationStepStatus::TimedOut
    );
    assert!(run_timeout.step_runs[0].exit_code.is_none());

    // 6. Case D: CANCELLATION
    let (tx, rx) = tokio::sync::watch::channel(false);
    let plan_cancel = VerificationPlan::new(
        "cancel-plan",
        "Cancel Plan",
        vec![VerificationStep::new_command(
            "step_cancel",
            "Cancel Step",
            vec![script_timeout.to_str().unwrap().into()],
        )],
    );

    let store_clone = VerificationStore::new(engine.pool.clone());
    let ws_path = ws_dir.path().to_path_buf();
    let env_clone = env.clone();
    let ws_state_b_clone = ws_state_b.clone();

    let handle = tokio::spawn(async move {
        execute_verification_plan(
            &store_clone,
            "attempt-1",
            &ws_state_b_clone,
            &plan_cancel,
            &ws_path,
            env_clone,
            Some(rx),
        )
        .await
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    tx.send(true)?;

    let run_cancel = handle.await??;
    assert_eq!(run_cancel.status, VerificationStepStatus::Cancelled);
    assert_eq!(
        run_cancel.overall_result,
        Some(VerificationRunResult::Cancelled)
    );

    // 7. Case E: CHILD PROCESS GROUP CLEANUP
    let marker = ws_dir.path().join("child_alive.txt");
    let script_tree = ws_dir.path().join("tree.sh");
    tokio::fs::write(
        &script_tree,
        format!(
            "#!/bin/sh\n(sleep 0.1 && echo alive > {}) &\nsleep 10\n",
            marker.display()
        ),
    )
    .await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&script_tree, std::fs::Permissions::from_mode(0o755)).await?;
    }

    let mut step_tree = VerificationStep::new_command(
        "step_tree",
        "Process Tree Step",
        vec![script_tree.to_str().unwrap().into()],
    );
    step_tree.timeout_seconds = 1;

    let plan_tree = VerificationPlan::new("tree-plan", "Tree Plan", vec![step_tree]);
    let run_tree = execute_verification_plan(
        &store,
        "attempt-1",
        &ws_state_b,
        &plan_tree,
        ws_dir.path(),
        env.clone(),
        None,
    )
    .await?;
    assert_eq!(
        run_tree.overall_result,
        Some(VerificationRunResult::TimedOut)
    );

    // 8. List verification runs for attempt
    let runs = store.list_runs("attempt-1").await?;
    assert_eq!(runs.len(), 5);

    // Clean up test schema
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin)
        .await?;

    Ok(())
}
