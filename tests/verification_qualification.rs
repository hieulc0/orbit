use anyhow::{Context, Result};
use orbit::{
    engine::Engine,
    model::id,
    verification::{
        EnvironmentIdentity, MAX_INLINE_OUTPUT_BYTES, VerificationPlan, VerificationRunResult,
        VerificationStep, VerificationStepStatus, VerificationStore, WorkspaceState,
        execute_verification_command_isolated, execute_verification_plan,
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
        architecture: "x86_64".into(),
        os: "linux".into(),
        orbit_version: "0.1.0".into(),
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
        architecture: "x86_64".into(),
        os: "linux".into(),
        orbit_version: "0.1.0".into(),
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
        architecture: "x86_64".into(),
        os: "linux".into(),
        orbit_version: "0.1.0".into(),
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
        architecture: "x86_64".into(),
        os: "linux".into(),
        orbit_version: "0.1.0".into(),
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
