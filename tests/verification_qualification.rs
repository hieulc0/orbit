use anyhow::{Context, Result};
use orbit::{
    engine::Engine,
    model::id,
    verification::{
        AllowedCommand, EnvironmentIdentity, MAX_INLINE_OUTPUT_BYTES, VerificationCachePolicy,
        VerificationEnvironmentPolicy, VerificationNetworkPolicy, VerificationPlan,
        VerificationPolicy, VerificationRunResult, VerificationStep, VerificationStepStatus,
        VerificationStore, WorkspaceState, execute_verification_command_isolated,
        execute_verification_plan, execute_verification_plan_with_policy,
    },
};
use sqlx::PgPool;
use std::time::Duration;

const PODMAN_IMAGE_RUST: &str = "docker.io/library/rust:latest";
const PODMAN_IMAGE_PYTHON: &str = "localhost/orbit-python:3.11-pytest";
const PODMAN_IMAGE_ALPINE: &str = "docker.io/library/alpine:latest";

async fn setup_db_store() -> Result<(Engine, VerificationStore, tempfile::TempDir)> {
    let base = std::env::var("ORBIT_TEST_DATABASE_URL")
        .context("set ORBIT_TEST_DATABASE_URL to a disposable PostgreSQL database")?;
    let admin = PgPool::connect(&base).await?;
    let schema = format!("orbit_qual_{}", id().replace('-', ""));
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await?;
    let separator = if base.contains('?') { '&' } else { '?' };
    let url = format!("{base}{separator}options=-csearch_path%3D{schema}");
    let home = tempfile::tempdir()?;

    let engine = Engine::connect(&url, home.path().join("artifacts"), 3).await?;
    let store = VerificationStore::new(engine.pool.clone());
    Ok((engine, store, home))
}

#[tokio::test]
#[ignore = "requires podman sandbox"]
async fn test_b1_qualification_host_isolation_and_security_escape() -> Result<()> {
    let ws_dir = tempfile::tempdir()?;
    let canary = ws_dir.path().join("canary.txt");
    tokio::fs::write(&canary, "canary-secret-12345").await?;

    let step = VerificationStep::new_command(
        "escape_probe",
        "Isolation Escape Probe",
        vec![
            "sh".into(),
            "-c".into(),
            r#"
            echo "--- PROBING FILESYSTEM ISOLATION ---"
            # 1. Host private credentials and home directory must not be present
            test -e /home/hieulc/.orbit/private && exit 101
            # 2. Host docker / podman socket must not be accessible
            test -e /var/run/docker.sock && exit 102
            test -e /var/run/podman/podman.sock && exit 103
            # 3. Read-only root filesystem check
            touch /etc/hacked 2>/dev/null && exit 104
            # 4. Workspace canary verify
            test -f /workspace/canary.txt || exit 105
            cat /workspace/canary.txt | grep -q canary-secret-12345 || exit 106
            echo "ISOLATION_VERIFIED"
            exit 0
            "#
            .into(),
        ],
    );

    let capture = execute_verification_command_isolated(
        &step,
        ws_dir.path(),
        Some(Duration::from_secs(20)),
        None,
        Some(PODMAN_IMAGE_ALPINE),
        None,
    )
    .await?;

    assert_eq!(
        capture.exit_code,
        Some(0),
        "Probe failed with exit code {:?}: stderr: {}",
        capture.exit_code,
        String::from_utf8_lossy(&capture.stderr_bytes)
    );
    assert!(
        String::from_utf8_lossy(&capture.stdout_bytes).contains("ISOLATION_VERIFIED"),
        "stdout missing verification marker"
    );
    assert!(!capture.timed_out);
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b1_qualification_large_output_accounting_and_truncation() -> Result<()> {
    let (engine, store, _home) = setup_db_store().await?;
    let ws_dir = tempfile::tempdir()?;

    // Generates >120,000 bytes on stdout and stderr without single large chunks
    let step = VerificationStep::new_command(
        "large_output_flood",
        "Output Flooder",
        vec![
            "python3".into(),
            "-c".into(),
            "import sys; sys.stdout.write('A' * 125000); sys.stderr.write('B' * 125000); sys.stdout.flush(); sys.stderr.flush()".into(),
        ],
    );

    let plan = VerificationPlan::new("large-output-plan", "Large Output Plan", vec![step]);
    store.save_plan(&plan).await?;

    let env = EnvironmentIdentity {
        execution_profile: "sandboxed-container".into(),
        isolation: "rootless-podman".into(),
        runtime_image: Some(PODMAN_IMAGE_PYTHON.into()),
        runtime_image_digest: None,
        oci_runtime: Some("podman".into()),
        network_policy: VerificationNetworkPolicy::None,
        cache_policy: VerificationCachePolicy::Clean,
        environment_policy_digest: None,
        integration_environment_digest: None,
        architecture: "x86_64".into(),
        os: "linux".into(),
        orbit_version: "0.1.0".into(),
        browser_verification_digest: None,
        browser_runtime_image_digest: None,
        regression_policy_digest: None,
        selection_digest: None,
    };

    let ws_state = WorkspaceState::compute_from_parts("base-flood", "head-flood", None);
    let run = execute_verification_plan(
        &store,
        "attempt-flood-1",
        &ws_state,
        &plan,
        ws_dir.path(),
        env,
        None,
    )
    .await?;

    assert_eq!(run.overall_result, Some(VerificationRunResult::Passed));
    let step_run = &run.step_runs[0];
    assert_eq!(step_run.status, VerificationStepStatus::Passed);
    assert!(
        step_run.stdout_bytes >= 125000,
        "stdout_bytes: {}",
        step_run.stdout_bytes
    );
    assert!(
        step_run.stderr_bytes >= 125000,
        "stderr_bytes: {}",
        step_run.stderr_bytes
    );
    assert!(step_run.stdout_truncated, "stdout must be marked truncated");
    assert!(step_run.stderr_truncated, "stderr must be marked truncated");
    assert!(
        step_run.stdout_preview.as_ref().unwrap().len() <= MAX_INLINE_OUTPUT_BYTES,
        "preview cap violated"
    );
    assert!(
        step_run.stderr_preview.as_ref().unwrap().len() <= MAX_INLINE_OUTPUT_BYTES,
        "preview cap violated"
    );

    // Also verify persistence in PostgreSQL
    let reloaded = store.get_run(&run.id).await?.unwrap();
    let reloaded_step = &reloaded.step_runs[0];
    assert!(reloaded_step.stdout_truncated);
    assert!(reloaded_step.stderr_truncated);
    assert!(reloaded_step.stdout_bytes >= 125000);
    assert!(reloaded_step.stderr_bytes >= 125000);
    assert!(reloaded_step.stdout_preview.as_ref().unwrap().len() <= MAX_INLINE_OUTPUT_BYTES);

    engine.pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b1_qualification_real_rust_fixture() -> Result<()> {
    let (engine, store, _home) = setup_db_store().await?;
    let ws_dir = tempfile::tempdir()?;

    // Create a real minimal cargo package
    let cargo_toml = r#"
[package]
name = "fixture_crate"
version = "0.1.0"
edition = "2021"

[dependencies]
"#;
    let main_rs_pass = r#"
fn add(a: i32, b: i32) -> i32 {
    a + b
}

fn main() {
    println!("result = {}", add(2, 3));
}

#[test]
fn test_addition() {
    assert_eq!(add(2, 3), 5);
}
"#;
    let src_dir = ws_dir.path().join("src");
    tokio::fs::create_dir_all(&src_dir).await?;
    tokio::fs::write(ws_dir.path().join("Cargo.toml"), cargo_toml).await?;
    tokio::fs::write(src_dir.join("main.rs"), main_rs_pass).await?;

    let plan = VerificationPlan::new(
        "rust_cargo_test",
        "Rust Cargo Test Plan",
        vec![VerificationStep::new_command(
            "cargo_test",
            "Cargo Test",
            vec!["cargo".into(), "test".into(), "--offline".into()],
        )],
    );
    store.save_plan(&plan).await?;

    let env = EnvironmentIdentity {
        execution_profile: "sandboxed-container".into(),
        isolation: "rootless-podman".into(),
        runtime_image: Some(PODMAN_IMAGE_RUST.into()),
        runtime_image_digest: None,
        oci_runtime: Some("podman".into()),
        network_policy: VerificationNetworkPolicy::None,
        cache_policy: VerificationCachePolicy::Clean,
        environment_policy_digest: None,
        integration_environment_digest: None,
        architecture: "x86_64".into(),
        os: "linux".into(),
        orbit_version: "0.1.0".into(),
        browser_verification_digest: None,
        browser_runtime_image_digest: None,
        regression_policy_digest: None,
        selection_digest: None,
    };

    let ws_state_pass = WorkspaceState::compute_from_parts("base-1", "head-1", Some("sha256-pass"));
    let run_pass = execute_verification_plan(
        &store,
        "attempt-rust-1",
        &ws_state_pass,
        &plan,
        ws_dir.path(),
        env.clone(),
        None,
    )
    .await?;

    assert_eq!(run_pass.status, VerificationStepStatus::Passed);
    assert_eq!(run_pass.overall_result, Some(VerificationRunResult::Passed));
    assert_eq!(run_pass.step_runs[0].status, VerificationStepStatus::Passed);
    assert_eq!(run_pass.step_runs[0].exit_code, Some(0));

    // Now mutate test to fail
    let main_rs_fail =
        main_rs_pass.replace("assert_eq!(add(2, 3), 5);", "assert_eq!(add(2, 3), 999);");
    tokio::fs::write(src_dir.join("main.rs"), main_rs_fail).await?;

    let ws_state_fail = WorkspaceState::compute_from_parts("base-1", "head-2", Some("sha256-fail"));
    let run_fail = execute_verification_plan(
        &store,
        "attempt-rust-2",
        &ws_state_fail,
        &plan,
        ws_dir.path(),
        env,
        None,
    )
    .await?;

    assert_eq!(run_fail.status, VerificationStepStatus::Failed);
    assert_eq!(run_fail.overall_result, Some(VerificationRunResult::Failed));
    assert_eq!(run_fail.step_runs[0].status, VerificationStepStatus::Failed);
    assert_ne!(run_fail.step_runs[0].exit_code, Some(0));

    engine.pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b1_qualification_real_python_fixture() -> Result<()> {
    let (engine, store, _home) = setup_db_store().await?;
    let ws_dir = tempfile::tempdir()?;

    let pyproject = r#"
[project]
name = "fixture-py"
version = "0.1.0"
"#;
    let test_py_pass = r#"
def test_calc_pass():
    assert 10 + 20 == 30
"#;
    let test_file = ws_dir.path().join("test_calc.py");
    tokio::fs::write(ws_dir.path().join("pyproject.toml"), pyproject).await?;
    tokio::fs::write(&test_file, test_py_pass).await?;

    let plan = VerificationPlan::new(
        "python_pytest_plan",
        "Python Pytest Plan",
        vec![VerificationStep::new_command(
            "pytest_step",
            "Pytest Run",
            vec![
                "pytest".into(),
                "-q".into(),
                "/workspace/test_calc.py".into(),
            ],
        )],
    );
    store.save_plan(&plan).await?;

    let env = EnvironmentIdentity {
        execution_profile: "sandboxed-container".into(),
        isolation: "rootless-podman".into(),
        runtime_image: Some(PODMAN_IMAGE_PYTHON.into()),
        runtime_image_digest: None,
        oci_runtime: Some("podman".into()),
        network_policy: VerificationNetworkPolicy::None,
        cache_policy: VerificationCachePolicy::Clean,
        environment_policy_digest: None,
        integration_environment_digest: None,
        architecture: "x86_64".into(),
        os: "linux".into(),
        orbit_version: "0.1.0".into(),
        browser_verification_digest: None,
        browser_runtime_image_digest: None,
        regression_policy_digest: None,
        selection_digest: None,
    };

    let ws_state_pass =
        WorkspaceState::compute_from_parts("base-py-1", "head-py-1", Some("diff-py-pass"));
    let run_pass = execute_verification_plan(
        &store,
        "attempt-py-1",
        &ws_state_pass,
        &plan,
        ws_dir.path(),
        env.clone(),
        None,
    )
    .await?;

    assert_eq!(run_pass.status, VerificationStepStatus::Passed);
    assert_eq!(run_pass.overall_result, Some(VerificationRunResult::Passed));
    assert_eq!(run_pass.step_runs[0].status, VerificationStepStatus::Passed);
    assert_eq!(run_pass.step_runs[0].exit_code, Some(0));

    // Mutate to fail
    let test_py_fail = r#"
def test_calc_fail():
    assert 10 + 20 == 999
"#;
    tokio::fs::write(&test_file, test_py_fail).await?;

    let ws_state_fail =
        WorkspaceState::compute_from_parts("base-py-1", "head-py-2", Some("diff-py-fail"));
    let run_fail = execute_verification_plan(
        &store,
        "attempt-py-2",
        &ws_state_fail,
        &plan,
        ws_dir.path(),
        env,
        None,
    )
    .await?;

    assert_eq!(run_fail.status, VerificationStepStatus::Failed);
    assert_eq!(run_fail.overall_result, Some(VerificationRunResult::Failed));
    assert_eq!(run_fail.step_runs[0].status, VerificationStepStatus::Failed);
    assert_ne!(run_fail.step_runs[0].exit_code, Some(0));

    engine.pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn test_b1_qualification_restart_durability() -> Result<()> {
    let base = std::env::var("ORBIT_TEST_DATABASE_URL")
        .context("set ORBIT_TEST_DATABASE_URL to a disposable PostgreSQL database")?;
    let admin = PgPool::connect(&base).await?;
    let schema = format!("orbit_durability_{}", id().replace('-', ""));
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await?;
    let separator = if base.contains('?') { '&' } else { '?' };
    let url = format!("{base}{separator}options=-csearch_path%3D{schema}");
    let home = tempfile::tempdir()?;

    let plan_id = "durable-plan";
    let plan = VerificationPlan::new(
        plan_id,
        "Durable Plan",
        vec![VerificationStep::new_command(
            "step_durable",
            "Durable Step",
            vec!["echo".into(), "survives restart".into()],
        )],
    );

    let ws_state = WorkspaceState::compute_from_parts("base-dur-1", "head-dur-1", None);
    let env = EnvironmentIdentity {
        execution_profile: "local-operator".into(),
        isolation: "process-group".into(),
        runtime_image: None,
        runtime_image_digest: None,
        oci_runtime: None,
        network_policy: VerificationNetworkPolicy::None,
        cache_policy: VerificationCachePolicy::Clean,
        environment_policy_digest: None,
        integration_environment_digest: None,
        architecture: "x86_64".into(),
        os: "linux".into(),
        orbit_version: "0.1.0".into(),
        browser_verification_digest: None,
        browser_runtime_image_digest: None,
        regression_policy_digest: None,
        selection_digest: None,
    };

    let run_id = {
        let engine = Engine::connect(&url, home.path().join("artifacts"), 3).await?;
        let store = VerificationStore::new(engine.pool.clone());
        store.save_plan(&plan).await?;
        let ws_dir = tempfile::tempdir()?;
        let run = execute_verification_plan(
            &store,
            "attempt-dur-1",
            &ws_state,
            &plan,
            ws_dir.path(),
            env.clone(),
            None,
        )
        .await?;
        assert_eq!(run.overall_result, Some(VerificationRunResult::Passed));
        engine.pool.close().await;
        run.id
    };

    // Reopen with completely new connection pool & new engine instance
    {
        let engine2 = Engine::connect(&url, home.path().join("artifacts_new"), 3).await?;
        let store2 = VerificationStore::new(engine2.pool.clone());
        let reloaded = store2
            .get_run(&run_id)
            .await?
            .context("run not found after restart")?;
        assert_eq!(reloaded.id, run_id);
        assert_eq!(reloaded.attempt_id, "attempt-dur-1");
        assert_eq!(reloaded.workspace_state_id, ws_state.state_id);
        assert_eq!(reloaded.overall_result, Some(VerificationRunResult::Passed));
        assert_eq!(reloaded.step_runs.len(), 1);
        assert_eq!(reloaded.step_runs[0].step_id, "step_durable");
        assert_eq!(reloaded.step_runs[0].status, VerificationStepStatus::Passed);
        assert!(
            reloaded.step_runs[0]
                .stdout_preview
                .as_ref()
                .unwrap()
                .contains("survives restart")
        );
        engine2.pool.close().await;
    }

    Ok(())
}

#[tokio::test]
async fn test_b1_qualification_empty_or_no_test_rejection() -> Result<()> {
    // 1. Empty plan rejected
    let empty_plan = VerificationPlan {
        id: "empty".into(),
        version: 1,
        name: "Empty".into(),
        steps: vec![],
    };
    assert!(empty_plan.validate().is_err());

    // 2. Plan with zero required steps rejected (no tests != pass)
    let mut no_required_step =
        VerificationStep::new_command("opt", "Optional", vec!["echo".into()]);
    no_required_step.required = false;
    let non_testing_plan = VerificationPlan {
        id: "no-tests".into(),
        version: 1,
        name: "No Tests".into(),
        steps: vec![no_required_step],
    };
    let err = non_testing_plan.validate().unwrap_err();
    assert!(
        err.to_string()
            .contains("at least one required verification step")
    );

    Ok(())
}

// ======================================================================
// PHASE B2 QUALIFICATION TESTS
// ======================================================================

#[tokio::test]
#[ignore = "requires podman sandbox"]
async fn test_b2_qualification_host_env_leak_and_clean_home() -> Result<()> {
    let ws_dir = tempfile::tempdir()?;
    unsafe {
        std::env::set_var("ORBIT_SECRET_CANARY", "should-not-be-visible");
    }

    let step = VerificationStep::new_command(
        "clean_env_probe",
        "Clean Environment & HOME Probe",
        vec![
            "sh".into(),
            "-c".into(),
            r#"
            echo "--- PROBING ENVIRONMENT CLEANLINESS ---"
            # 1. Host variable must not leak
            if [ -n "$ORBIT_SECRET_CANARY" ]; then
                echo "LEAK: ORBIT_SECRET_CANARY=$ORBIT_SECRET_CANARY"
                exit 110
            fi

            # 2. HOME must be isolated /tmp/orbit-home
            if [ "$HOME" != "/tmp/orbit-home" ]; then
                echo "INVALID HOME: $HOME"
                exit 111
            fi

            # 3. Clean HOME probe: credential & config paths must be absent
            test -e "$HOME/.ssh" && exit 112
            test -e "$HOME/.gitconfig" && exit 113
            test -e "$HOME/.cargo/credentials" && exit 114
            test -e "$HOME/.npmrc" && exit 115
            test -e "$HOME/.pypirc" && exit 116
            test -e "$HOME/.config" && exit 117
            test -e "$HOME/.aws" && exit 118
            test -e "$HOME/.kube" && exit 119
            test -e "$HOME/.docker" && exit 120
            test -e "$HOME/.orbit" && exit 121

            # 4. Standard clean environment defaults must be present
            test "$CI" = "1" || exit 122
            test "$LANG" = "C.UTF-8" || exit 123
            test "$LC_ALL" = "C.UTF-8" || exit 124
            test "$TERM" = "dumb" || exit 125
            test "$ORBIT_VERIFICATION" = "1" || exit 126

            echo "CLEAN_ENVIRONMENT_VERIFIED"
            exit 0
            "#
            .into(),
        ],
    );

    let capture = execute_verification_command_isolated(
        &step,
        ws_dir.path(),
        Some(Duration::from_secs(20)),
        None,
        Some(PODMAN_IMAGE_ALPINE),
        None,
    )
    .await?;

    assert_eq!(
        capture.exit_code,
        Some(0),
        "Probe failed with exit code {:?}: stderr: {}",
        capture.exit_code,
        String::from_utf8_lossy(&capture.stderr_bytes)
    );
    assert!(
        String::from_utf8_lossy(&capture.stdout_bytes).contains("CLEAN_ENVIRONMENT_VERIFIED"),
        "stdout missing verification marker"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires podman sandbox"]
async fn test_b2_qualification_explicit_env_injection_and_allowlist() -> Result<()> {
    let ws_dir = tempfile::tempdir()?;

    unsafe {
        std::env::set_var("HOST_INHERITED_ALLOWED", "from-host-env");
        std::env::set_var("HOST_INHERITED_DENIED", "should-be-denied");
    }

    let mut env_policy = VerificationEnvironmentPolicy::clean();
    env_policy.inherit.push("HOST_INHERITED_ALLOWED".into());
    env_policy
        .set
        .insert("ORBIT_TEST_VALUE".into(), "hello-b2".into());
    env_policy.deny.push("HOST_INHERITED_DENIED".into());

    let step = VerificationStep::new_command(
        "explicit_env_probe",
        "Explicit Env Probe",
        vec![
            "sh".into(),
            "-c".into(),
            r#"
            test "$ORBIT_TEST_VALUE" = "hello-b2" || exit 131
            test "$HOST_INHERITED_ALLOWED" = "from-host-env" || exit 132
            test -z "$HOST_INHERITED_DENIED" || exit 133
            echo "ENV_ALLOWLIST_VERIFIED"
            exit 0
            "#
            .into(),
        ],
    );

    let capture = execute_verification_command_isolated(
        &step,
        ws_dir.path(),
        Some(Duration::from_secs(20)),
        None,
        Some(PODMAN_IMAGE_ALPINE),
        Some(&env_policy),
    )
    .await?;

    assert_eq!(capture.exit_code, Some(0));
    assert!(String::from_utf8_lossy(&capture.stdout_bytes).contains("ENV_ALLOWLIST_VERIFIED"));
    Ok(())
}

#[tokio::test]
#[ignore = "requires podman sandbox"]
async fn test_b2_qualification_network_remains_unavailable() -> Result<()> {
    let ws_dir = tempfile::tempdir()?;

    let step = VerificationStep::new_command(
        "net_probe",
        "Network Isolation Probe",
        vec![
            "sh".into(),
            "-c".into(),
            r#"
            # In a network=none container, loopback is the only interface (or no route to external)
            # Connecting to any arbitrary non-loopback IP must immediately fail (network unreachable)
            nc -z -w 1 8.8.8.8 53 2>/dev/null && exit 141
            nc -z -w 1 1.1.1.1 80 2>/dev/null && exit 142
            echo "NETWORK_NONE_VERIFIED"
            exit 0
            "#
            .into(),
        ],
    );

    let capture = execute_verification_command_isolated(
        &step,
        ws_dir.path(),
        Some(Duration::from_secs(20)),
        None,
        Some(PODMAN_IMAGE_ALPINE),
        None,
    )
    .await?;

    assert_eq!(capture.exit_code, Some(0));
    assert!(String::from_utf8_lossy(&capture.stdout_bytes).contains("NETWORK_NONE_VERIFIED"));
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b2_qualification_policy_mutation_invalidates_qualification() -> Result<()> {
    let (engine, store, _home) = setup_db_store().await?;
    let ws_dir = tempfile::tempdir()?;

    let mut policy_v1 = VerificationPolicy::new("pol-test", "Policy Test V1");
    policy_v1.version = 1;
    policy_v1.required_steps = vec!["check1".into()];
    policy_v1.allowed_commands = vec![AllowedCommand::exact("echo")];
    store.save_policy(&policy_v1).await?;

    let plan = VerificationPlan::new(
        "plan-pol",
        "Plan",
        vec![VerificationStep::new_command(
            "check1",
            "Check 1",
            vec!["echo".into(), "ok".into()],
        )],
    );
    store.save_plan(&plan).await?;

    let env = EnvironmentIdentity {
        execution_profile: "sandboxed-container".into(),
        isolation: "rootless-podman".into(),
        runtime_image: Some(PODMAN_IMAGE_ALPINE.into()),
        runtime_image_digest: None,
        oci_runtime: Some("podman".into()),
        network_policy: VerificationNetworkPolicy::None,
        cache_policy: VerificationCachePolicy::Clean,
        environment_policy_digest: Some(policy_v1.environment_policy.digest()),
        integration_environment_digest: None,

        architecture: "x86_64".into(),
        os: "linux".into(),
        orbit_version: "0.1.0".into(),
        browser_verification_digest: None,
        browser_runtime_image_digest: None,
        regression_policy_digest: None,
        selection_digest: None,
    };

    let ws_state = WorkspaceState::compute_from_parts("base-p1", "head-p1", None);

    let run = execute_verification_plan_with_policy(
        &store,
        "attempt-p1",
        &ws_state,
        &plan,
        ws_dir.path(),
        env.clone(),
        Some(&policy_v1),
        None,
    )
    .await?;

    assert_eq!(run.overall_result, Some(VerificationRunResult::Passed));

    // Under policy v1, workspace is qualified!
    let qualified_v1 = store
        .check_workspace_qualification(&ws_state.state_id, &policy_v1, Some(&env))
        .await?;
    assert!(
        qualified_v1.is_some(),
        "workspace must qualify under policy v1"
    );

    // Mutate policy to v2 (requires check2 in addition to check1)
    let mut policy_v2 = policy_v1.clone();
    policy_v2.version = 2;
    policy_v2.required_steps.push("check2".into());
    store.save_policy(&policy_v2).await?;

    // Under policy v2, the exact same workspace state does NOT qualify!
    let qualified_v2 = store
        .check_workspace_qualification(&ws_state.state_id, &policy_v2, Some(&env))
        .await?;
    assert!(
        qualified_v2.is_none(),
        "workspace state must NOT qualify under mutated policy v2"
    );

    engine.pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b2_qualification_environment_identity_mutation_invalidates_qualification()
-> Result<()> {
    let (engine, store, _home) = setup_db_store().await?;
    let ws_dir = tempfile::tempdir()?;

    let policy = VerificationPolicy::new("pol-env-test", "Policy Env Test");
    store.save_policy(&policy).await?;

    let plan = VerificationPlan::new(
        "plan-env",
        "Plan",
        vec![VerificationStep::new_command(
            "step1",
            "Step 1",
            vec!["echo".into(), "hello".into()],
        )],
    );
    store.save_plan(&plan).await?;

    let env_a = EnvironmentIdentity {
        execution_profile: "sandboxed-container".into(),
        isolation: "rootless-podman".into(),
        runtime_image: Some(PODMAN_IMAGE_ALPINE.into()),
        runtime_image_digest: Some("sha256:digest-alpha".into()),
        oci_runtime: Some("podman".into()),
        network_policy: VerificationNetworkPolicy::None,
        cache_policy: VerificationCachePolicy::Clean,
        environment_policy_digest: Some(policy.environment_policy.digest()),
        integration_environment_digest: None,

        architecture: "x86_64".into(),
        os: "linux".into(),
        orbit_version: "0.1.0".into(),
        browser_verification_digest: None,
        browser_runtime_image_digest: None,
        regression_policy_digest: None,
        selection_digest: None,
    };

    let ws_state = WorkspaceState::compute_from_parts("base-e1", "head-e1", None);

    let run = execute_verification_plan_with_policy(
        &store,
        "attempt-e1",
        &ws_state,
        &plan,
        ws_dir.path(),
        env_a.clone(),
        Some(&policy),
        None,
    )
    .await?;

    assert_eq!(run.overall_result, Some(VerificationRunResult::Passed));

    // Matches env_a
    let qual_a = store
        .check_workspace_qualification(&ws_state.state_id, &policy, Some(&env_a))
        .await?;
    assert!(qual_a.is_some());

    // Fails under different image digest requirement env_b
    let mut env_b = env_a.clone();
    env_b.runtime_image_digest = Some("sha256:digest-beta".into());

    let qual_b = store
        .check_workspace_qualification(&ws_state.state_id, &policy, Some(&env_b))
        .await?;
    assert!(
        qual_b.is_none(),
        "prior run under digest alpha must not satisfy digest beta requirement"
    );

    engine.pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b2_qualification_required_step_policy_enforcement() -> Result<()> {
    let (engine, store, _home) = setup_db_store().await?;
    let ws_dir = tempfile::tempdir()?;

    // Policy requires both "fmt" and "test"
    let mut policy = VerificationPolicy::new("pol-req", "Required Steps Policy");
    policy.required_steps = vec!["fmt".into(), "test".into()];
    store.save_policy(&policy).await?;

    // Plan that only has "fmt"
    let plan_partial = VerificationPlan::new(
        "plan-partial",
        "Partial Plan",
        vec![VerificationStep::new_command(
            "fmt",
            "Format",
            vec!["echo".into(), "fmt ok".into()],
        )],
    );
    store.save_plan(&plan_partial).await?;

    let env = EnvironmentIdentity {
        execution_profile: "sandboxed-container".into(),
        isolation: "rootless-podman".into(),
        runtime_image: Some(PODMAN_IMAGE_ALPINE.into()),
        runtime_image_digest: None,
        oci_runtime: Some("podman".into()),
        network_policy: VerificationNetworkPolicy::None,
        cache_policy: VerificationCachePolicy::Clean,
        environment_policy_digest: Some(policy.environment_policy.digest()),
        integration_environment_digest: None,

        architecture: "x86_64".into(),
        os: "linux".into(),
        orbit_version: "0.1.0".into(),
        browser_verification_digest: None,
        browser_runtime_image_digest: None,
        regression_policy_digest: None,
        selection_digest: None,
    };

    let ws_state = WorkspaceState::compute_from_parts("base-req", "head-req", None);

    // Attempting to execute with policy must fail fast before running because plan is missing required step
    let res = execute_verification_plan_with_policy(
        &store,
        "attempt-req-fail",
        &ws_state,
        &plan_partial,
        ws_dir.path(),
        env,
        Some(&policy),
        None,
    )
    .await;

    assert!(res.is_err());
    assert!(res.unwrap_err().to_string().contains("mandated by policy"));

    engine.pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b2_qualification_restart_durability_with_policy() -> Result<()> {
    let base = std::env::var("ORBIT_TEST_DATABASE_URL")
        .context("set ORBIT_TEST_DATABASE_URL to a disposable PostgreSQL database")?;
    let admin = PgPool::connect(&base).await?;
    let schema = format!("orbit_qual_{}", id().replace('-', ""));
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await?;
    let separator = if base.contains('?') { '&' } else { '?' };
    let url = format!("{base}{separator}options=-csearch_path%3D{schema}");
    let home = tempfile::tempdir()?;

    let policy = VerificationPolicy::new("pol-restart", "Restart Durability Policy");
    let plan = VerificationPlan::new(
        "plan-restart",
        "Restart Plan",
        vec![VerificationStep::new_command(
            "step1",
            "Step 1",
            vec!["echo".into(), "restart-ok".into()],
        )],
    );

    let env = EnvironmentIdentity {
        execution_profile: "sandboxed-container".into(),
        isolation: "rootless-podman".into(),
        runtime_image: Some(PODMAN_IMAGE_ALPINE.into()),
        runtime_image_digest: None,
        oci_runtime: Some("podman".into()),
        network_policy: VerificationNetworkPolicy::None,
        cache_policy: VerificationCachePolicy::Clean,
        environment_policy_digest: Some(policy.environment_policy.digest()),
        integration_environment_digest: None,
        architecture: "x86_64".into(),
        os: "linux".into(),
        orbit_version: "0.1.0".into(),
        browser_verification_digest: None,
        browser_runtime_image_digest: None,
        regression_policy_digest: None,
        selection_digest: None,
    };

    let ws_state = WorkspaceState::compute_from_parts("base-rst", "head-rst", None);

    let run_id = {
        let engine = Engine::connect(&url, home.path().join("artifacts"), 3).await?;
        let store = VerificationStore::new(engine.pool.clone());
        store.save_policy(&policy).await?;
        store.save_plan(&plan).await?;
        let ws_dir = tempfile::tempdir()?;
        let run = execute_verification_plan_with_policy(
            &store,
            "attempt-rst",
            &ws_state,
            &plan,
            ws_dir.path(),
            env.clone(),
            Some(&policy),
            None,
        )
        .await?;
        assert_eq!(run.overall_result, Some(VerificationRunResult::Passed));
        engine.pool.close().await;
        run.id
    };

    // Reopen fresh connection pool to the EXACT SAME schema
    {
        let reconnected_engine =
            Engine::connect(&url, home.path().join("artifacts_new"), 3).await?;
        let reconnected_store = VerificationStore::new(reconnected_engine.pool.clone());

        let reloaded_run = reconnected_store.get_run(&run_id).await?.unwrap();
        assert_eq!(reloaded_run.policy_id, Some("pol-restart".into()));
        assert_eq!(reloaded_run.policy_digest, Some(policy.digest()));
        assert_eq!(
            reloaded_run.environment_identity.network_policy,
            VerificationNetworkPolicy::None
        );
        assert_eq!(
            reloaded_run.environment_identity.cache_policy,
            VerificationCachePolicy::Clean
        );

        // Verify qualification decision remains identical on reconnected store
        let qualified = reconnected_store
            .check_workspace_qualification(&ws_state.state_id, &policy, Some(&env))
            .await?;
        assert!(qualified.is_some());
        assert_eq!(qualified.unwrap().id, run_id);

        reconnected_engine.pool.close().await;
    }
    Ok(())
}
