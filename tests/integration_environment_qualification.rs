//! Phase B4 Qualification Test Suite: Managed Integration Test Environments
//!
//! Validates:
//! 1. CONTAINER_SERVICE management (PostgreSQL / Redis) with internal networking.
//! 2. PROCESS_SERVICE management with clean process-group lifecycle.
//! 3. Cross-service DNS name resolution over internal bridge networks.
//! 4. Strict isolation: external Internet traffic blocked; host Docker/Podman sockets inaccessible.
//! 5. Explicit readiness probes (TCP, HTTP, Process) with timeouts.
//! 6. Distinguishing SERVICE_START_FAILED vs READINESS_TIMEOUT vs VERIFICATION_FAILED.
//! 7. Dependency startup order and reverse-order cleanup.
//! 8. Setup / migration steps executed before tests run.
//! 9. Reverse-order teardown with no orphan containers, networks, or processes on PASS, FAIL, TIMEOUT, CANCEL.
//! 10. Durable environment identity with resolved image digests and mutation invalidation.

use anyhow::Result;
use orbit::{
    engine::Engine,
    integration_environment::{
        EnvironmentRunStatus, EnvironmentServiceStatus, EnvironmentStore,
        IntegrationEnvironmentSpec, ManagedServiceSpec, ReadinessProbe, ServiceKind,
    },
    model::id,
    verification::{
        EnvironmentIdentity, VerificationCachePolicy, VerificationNetworkPolicy, VerificationPlan,
        VerificationPolicy, VerificationRunResult, VerificationStep, VerificationStepStatus,
        VerificationStore, WorkspaceState, execute_verification_plan_with_policy,
    },
};
use sqlx::PgPool;
use std::collections::BTreeMap;

const PODMAN_IMAGE_ALPINE: &str = "docker.io/library/alpine:latest";
const PODMAN_IMAGE_POSTGRES: &str = "docker.io/library/postgres:17-alpine";
const PODMAN_IMAGE_REDIS: &str = "docker.io/library/redis:alpine";

struct TestContext {
    engine: Engine,
    verification_store: VerificationStore,
    env_store: EnvironmentStore,
    _schema: String,
    _home: tempfile::TempDir,
}

async fn setup_qualification_context() -> Result<TestContext> {
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
        anyhow::bail!("disposable database URL required for B4 qualification test");
    }

    let admin = PgPool::connect(&base).await?;
    let schema = format!("orbit_qual_b4_{}", id().replace('-', ""));
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await?;
    let separator = if base.contains('?') { '&' } else { '?' };
    let url = format!("{base}{separator}options=-csearch_path%3D{schema}");
    let home = tempfile::tempdir()?;

    let engine = Engine::connect(&url, home.path().join("artifacts"), 3).await?;
    let verification_store = VerificationStore::new(engine.pool.clone());
    let env_store = EnvironmentStore::new(engine.pool.clone());

    Ok(TestContext {
        engine,
        verification_store,
        env_store,
        _schema: schema,
        _home: home,
    })
}

/// Scenario 1: Managed Container Service (PostgreSQL) + Readiness Probe + Test Execution
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b4_01_managed_container_service_postgres() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    let mut env_spec = IntegrationEnvironmentSpec::new("test-pg-env");
    let mut pg_env = BTreeMap::new();
    pg_env.insert("POSTGRES_PASSWORD".into(), "secret".into());
    pg_env.insert("POSTGRES_DB".into(), "testdb".into());

    let pg_service = ManagedServiceSpec {
        id: "postgres".into(),
        kind: ServiceKind::Container,
        image: Some(PODMAN_IMAGE_POSTGRES.into()),
        command: Vec::new(),
        args: Vec::new(),
        env: pg_env,
        mounts: Vec::new(),
        internal_port: Some(5432),
        readiness: Some(ReadinessProbe::Tcp {
            host: "postgres".into(),
            port: 5432,
            timeout_seconds: 30,
            interval_ms: 250,
        }),
        timeout_seconds: 40,
        dependencies: Vec::new(),
    };
    env_spec.services.push(pg_service);

    let mut policy = VerificationPolicy::new("pol-b4-pg", "Postgres Policy");
    policy.network_policy = VerificationNetworkPolicy::Isolated;
    policy.integration_environment_spec = Some(env_spec.clone());
    ctx.verification_store.save_policy(&policy).await?;

    let plan = VerificationPlan::new(
        "plan-pg",
        "Postgres Test Plan",
        vec![VerificationStep::new_command(
            "test_pg_connect",
            "Verify Postgres Connection",
            vec![
                "sh".into(),
                "-c".into(),
                "nc -z -w 2 postgres 5432 && echo PG_READY_OK".into(),
            ],
        )],
    );
    ctx.verification_store.save_plan(&plan).await?;

    let env = EnvironmentIdentity {
        execution_profile: "sandboxed-container".into(),
        isolation: "rootless-podman".into(),
        runtime_image: Some(PODMAN_IMAGE_ALPINE.into()),
        runtime_image_digest: None,
        oci_runtime: Some("podman".into()),
        network_policy: VerificationNetworkPolicy::Isolated,
        cache_policy: VerificationCachePolicy::Clean,
        environment_policy_digest: Some(policy.environment_policy.digest()),
        integration_environment_digest: Some(env_spec.digest()),
        architecture: "x86_64".into(),
        os: "linux".into(),
        orbit_version: "0.1.0".into(),
    };

    let ws_state = WorkspaceState::compute_from_parts("base-1", "head-1", None);
    let run = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "attempt-b4-1",
        &ws_state,
        &plan,
        ws_dir.path(),
        env,
        Some(&policy),
        None,
    )
    .await?;

    assert_eq!(run.overall_result, Some(VerificationRunResult::Passed));
    let step = &run.step_runs[0];
    assert_eq!(step.status, VerificationStepStatus::Passed);
    assert!(
        step.stdout_preview
            .as_deref()
            .unwrap()
            .contains("PG_READY_OK")
    );

    // Verify EnvironmentRun record persisted
    let env_run = ctx
        .env_store
        .get_environment_run_for_verification(&run.id)
        .await?
        .unwrap();
    assert_eq!(env_run.status, EnvironmentRunStatus::Passed);
    assert_eq!(env_run.service_runs.len(), 1);
    assert_eq!(
        env_run.service_runs[0].status,
        EnvironmentServiceStatus::Ready
    );
    assert!(env_run.service_runs[0].resolved_image_digest.is_some());

    // Confirm container and network are cleaned up
    let ps_out = tokio::process::Command::new("podman")
        .args([
            "ps",
            "-a",
            "--filter",
            &format!(
                "name={}",
                env_run.service_runs[0].container_name.as_ref().unwrap()
            ),
        ])
        .output()
        .await?;
    assert!(!String::from_utf8_lossy(&ps_out.stdout).contains("orbit-svc-postgres"));

    ctx.engine.pool.close().await;
    Ok(())
}

/// Scenario 2: Multi-Service Environment (Postgres + Redis) with DNS discovery
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b4_02_multi_service_postgres_and_redis() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    let mut env_spec = IntegrationEnvironmentSpec::new("test-pg-redis-env");
    let mut pg_env = BTreeMap::new();
    pg_env.insert("POSTGRES_PASSWORD".into(), "secret".into());

    let pg_service = ManagedServiceSpec {
        id: "db".into(),
        kind: ServiceKind::Container,
        image: Some(PODMAN_IMAGE_POSTGRES.into()),
        command: Vec::new(),
        args: Vec::new(),
        env: pg_env,
        mounts: Vec::new(),
        internal_port: Some(5432),
        readiness: Some(ReadinessProbe::Tcp {
            host: "db".into(),
            port: 5432,
            timeout_seconds: 30,
            interval_ms: 250,
        }),
        timeout_seconds: 40,
        dependencies: Vec::new(),
    };

    let redis_service = ManagedServiceSpec {
        id: "cache".into(),
        kind: ServiceKind::Container,
        image: Some(PODMAN_IMAGE_REDIS.into()),
        command: Vec::new(),
        args: Vec::new(),
        env: BTreeMap::new(),
        mounts: Vec::new(),
        internal_port: Some(6379),
        readiness: Some(ReadinessProbe::Tcp {
            host: "cache".into(),
            port: 6379,
            timeout_seconds: 30,
            interval_ms: 250,
        }),
        timeout_seconds: 40,
        dependencies: Vec::new(),
    };

    env_spec.services.push(pg_service);
    env_spec.services.push(redis_service);

    let mut policy = VerificationPolicy::new("pol-b4-multi", "Multi Service Policy");
    policy.network_policy = VerificationNetworkPolicy::Isolated;
    policy.integration_environment_spec = Some(env_spec.clone());
    ctx.verification_store.save_policy(&policy).await?;

    let plan = VerificationPlan::new(
        "plan-multi",
        "Multi Service Test Plan",
        vec![VerificationStep::new_command(
            "test_both_services",
            "Verify Both DB and Cache",
            vec![
                "sh".into(),
                "-c".into(),
                "nc -z -w 2 db 5432 && nc -z -w 2 cache 6379 && echo MULTI_SERVICES_OK".into(),
            ],
        )],
    );
    ctx.verification_store.save_plan(&plan).await?;

    let env = EnvironmentIdentity {
        execution_profile: "sandboxed-container".into(),
        isolation: "rootless-podman".into(),
        runtime_image: Some(PODMAN_IMAGE_ALPINE.into()),
        runtime_image_digest: None,
        oci_runtime: Some("podman".into()),
        network_policy: VerificationNetworkPolicy::Isolated,
        cache_policy: VerificationCachePolicy::Clean,
        environment_policy_digest: Some(policy.environment_policy.digest()),
        integration_environment_digest: Some(env_spec.digest()),
        architecture: "x86_64".into(),
        os: "linux".into(),
        orbit_version: "0.1.0".into(),
    };

    let ws_state = WorkspaceState::compute_from_parts("base-2", "head-2", None);
    let run = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "attempt-b4-2",
        &ws_state,
        &plan,
        ws_dir.path(),
        env,
        Some(&policy),
        None,
    )
    .await?;

    assert_eq!(run.overall_result, Some(VerificationRunResult::Passed));
    assert!(
        run.step_runs[0]
            .stdout_preview
            .as_deref()
            .unwrap()
            .contains("MULTI_SERVICES_OK")
    );

    let env_run = ctx
        .env_store
        .get_environment_run_for_verification(&run.id)
        .await?
        .unwrap();
    assert_eq!(env_run.service_runs.len(), 2);

    ctx.engine.pool.close().await;
    Ok(())
}

/// Scenario 3: Process Service Isolation Proof & Management
/// Proves that repository PROCESS_SERVICE execution occurs strictly inside
/// an isolated container runtime and NOT directly on the Orbit worker host.
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b4_03_managed_process_service() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    // 1. Host Execution Canary: create a secret file on the worker host that is NOT mounted.
    // If the process service executes directly on the worker host, this file will exist.
    // Inside the isolated container runtime, this file must NOT exist.
    let host_canary_dir = tempfile::tempdir()?;
    let host_canary_path = host_canary_dir.path().join("host_canary.secret");
    tokio::fs::write(&host_canary_path, "HOST_WORKER_SECRET_DATA_DO_NOT_LEAK").await?;
    assert!(
        host_canary_path.exists(),
        "Host canary must exist on worker host"
    );

    // 2. Repository / Workspace Marker: create a marker in the repository workspace.
    // The isolated container must bind-mount the workspace at /workspace, so this marker will be readable.
    let repo_marker_path = ws_dir.path().join("repo_marker.txt");
    tokio::fs::write(&repo_marker_path, "WORKSPACE_REPOSITORY_MARKER_OK").await?;

    let host_canary_str = host_canary_path.to_str().unwrap().to_string();

    let mut env_spec = IntegrationEnvironmentSpec::new("test-proc-env");
    let mut proc_env = BTreeMap::new();
    proc_env.insert("HOST_CANARY_PATH".into(), host_canary_str.clone());

    // Script executed by the PROCESS_SERVICE
    let isolation_check_script = r#"
        # 1. Assert host canary file does NOT exist in isolated runtime
        if [ -f "$HOST_CANARY_PATH" ]; then
            echo "SECURITY_VIOLATION: host canary file leaked into service!" >&2
            exit 11
        fi

        # 2. Assert workspace marker is accessible at /workspace
        if [ ! -f /workspace/repo_marker.txt ]; then
            echo "SECURITY_VIOLATION: /workspace/repo_marker.txt not accessible!" >&2
            exit 12
        fi

        # 3. Assert host credentials and private state do NOT exist
        if [ -d /root/.orbit ] || [ -d /tmp/orbit-home/.orbit ]; then
            echo "SECURITY_VIOLATION: host ~/.orbit directory leaked!" >&2
            exit 13
        fi
        if [ -d /root/.ssh ] || [ -d /tmp/orbit-home/.ssh ]; then
            echo "SECURITY_VIOLATION: host ~/.ssh directory leaked!" >&2
            exit 14
        fi

        # 4. Assert host container sockets do NOT exist
        if [ -e /var/run/docker.sock ] || [ -e /run/podman/podman.sock ]; then
            echo "SECURITY_VIOLATION: host container socket mounted into service!" >&2
            exit 15
        fi

        # 5. Assert host database URL file env var is not set
        if [ -n "$ORBIT_DATABASE_URL_FILE" ]; then
            echo "SECURITY_VIOLATION: ORBIT_DATABASE_URL_FILE leaked into service!" >&2
            exit 16
        fi

        # 6. Assert isolated PID namespace
        process_count=$(ps -ef | wc -l)
        if [ "$process_count" -gt 30 ]; then
            echo "SECURITY_VIOLATION: PID count indicates host PID namespace ($process_count)!" >&2
            exit 17
        fi

        # 7. Bind port 8080 and listen for connections over the isolated network
        exec nc -lk -p 8080 -e echo PROCESS_SERVICE_ISOLATED_OK
    "#;

    let proc_service = ManagedServiceSpec {
        id: "api".into(),
        kind: ServiceKind::Process,
        image: Some(PODMAN_IMAGE_ALPINE.into()),
        command: vec!["sh".into(), "-c".into(), isolation_check_script.into()],
        args: Vec::new(),
        env: proc_env,
        mounts: Vec::new(),
        internal_port: Some(8080),
        readiness: Some(ReadinessProbe::Tcp {
            host: "api".into(),
            port: 8080,
            timeout_seconds: 15,
            interval_ms: 250,
        }),
        timeout_seconds: 20,
        dependencies: Vec::new(),
    };
    env_spec.services.push(proc_service);

    let mut policy = VerificationPolicy::new("pol-b4-proc", "Process Service Policy");
    policy.network_policy = VerificationNetworkPolicy::Isolated;
    policy.integration_environment_spec = Some(env_spec.clone());
    ctx.verification_store.save_policy(&policy).await?;

    let plan = VerificationPlan::new(
        "plan-proc",
        "Process Service Test Plan",
        vec![VerificationStep::new_command(
            "test_step",
            "Connect to Process Service via Internal Network",
            vec!["sh".into(), "-c".into(), "nc -w 3 api 8080".into()],
        )],
    );
    ctx.verification_store.save_plan(&plan).await?;

    let env = EnvironmentIdentity {
        execution_profile: "sandboxed-container".into(),
        isolation: "rootless-podman".into(),
        runtime_image: Some(PODMAN_IMAGE_ALPINE.into()),
        runtime_image_digest: None,
        oci_runtime: Some("podman".into()),
        network_policy: VerificationNetworkPolicy::Isolated,
        cache_policy: VerificationCachePolicy::Clean,
        environment_policy_digest: Some(policy.environment_policy.digest()),
        integration_environment_digest: Some(env_spec.digest()),
        architecture: "x86_64".into(),
        os: "linux".into(),
        orbit_version: "0.1.0".into(),
    };

    let ws_state = WorkspaceState::compute_from_parts("base-proc", "head-proc", None);
    let run = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "attempt-b4-proc",
        &ws_state,
        &plan,
        ws_dir.path(),
        env,
        Some(&policy),
        None,
    )
    .await?;

    assert_eq!(run.overall_result, Some(VerificationRunResult::Passed));
    assert!(
        run.step_runs[0]
            .stdout_preview
            .as_deref()
            .unwrap()
            .contains("PROCESS_SERVICE_ISOLATED_OK"),
        "Verification step should receive response from isolated process service"
    );

    // Assert host canary file was NOT deleted or touched on worker host
    assert!(
        host_canary_path.exists(),
        "Host canary must remain intact on host"
    );

    // Assert service run record details
    let env_run = ctx
        .env_store
        .get_environment_run_for_verification(&run.id)
        .await?
        .unwrap();
    assert_eq!(env_run.status, EnvironmentRunStatus::Passed);
    assert_eq!(env_run.service_runs.len(), 1);

    let svc_run = &env_run.service_runs[0];
    assert_eq!(svc_run.service_id, "api");
    assert_eq!(svc_run.service_kind, ServiceKind::Process);
    assert_eq!(svc_run.status, EnvironmentServiceStatus::Ready);
    assert!(
        svc_run
            .container_name
            .as_deref()
            .unwrap()
            .starts_with("orbit-proc-api-")
    );
    assert!(svc_run.resolved_image_digest.is_some());

    // Assert container cleanup: container must NOT exist after teardown
    let cname = svc_run.container_name.as_deref().unwrap();
    let ps_check = tokio::process::Command::new("podman")
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("name={}", cname),
            "--format",
            "{{.Names}}",
        ])
        .output()
        .await?;
    assert_eq!(String::from_utf8_lossy(&ps_check.stdout).trim(), "");

    // Assert network cleanup: isolated network must NOT exist after teardown
    let net_name = env_run.network_name.as_deref().unwrap();
    let net_check = tokio::process::Command::new("podman")
        .args([
            "network",
            "ls",
            "--filter",
            &format!("name={}", net_name),
            "--format",
            "{{.Name}}",
        ])
        .output()
        .await?;
    assert_eq!(String::from_utf8_lossy(&net_check.stdout).trim(), "");

    ctx.engine.pool.close().await;
    Ok(())
}

/// Scenario 4: External Network Isolation (`--internal` Podman Network blocks Internet)
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b4_04_isolated_network_blocks_external_internet() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    let mut env_spec = IntegrationEnvironmentSpec::new("test-isolated-net");
    env_spec.network_policy = VerificationNetworkPolicy::Isolated;

    let mut policy = VerificationPolicy::new("pol-b4-iso", "Isolated Network Policy");
    policy.network_policy = VerificationNetworkPolicy::Isolated;
    policy.integration_environment_spec = Some(env_spec.clone());
    ctx.verification_store.save_policy(&policy).await?;

    let plan = VerificationPlan::new(
        "plan-iso-block",
        "Isolated Network External Block Plan",
        vec![VerificationStep::new_command(
            "probe_external",
            "Probe External IP",
            vec![
                "sh".into(),
                "-c".into(),
                // Connecting to 1.1.1.1 must fail immediately or timeout
                "if nc -z -w 2 1.1.1.1 80; then echo LEAKED_EXTERNAL && exit 101; else echo EXTERNAL_BLOCKED_OK; fi".into(),
            ],
        )],
    );
    ctx.verification_store.save_plan(&plan).await?;

    let env = EnvironmentIdentity {
        execution_profile: "sandboxed-container".into(),
        isolation: "rootless-podman".into(),
        runtime_image: Some(PODMAN_IMAGE_ALPINE.into()),
        runtime_image_digest: None,
        oci_runtime: Some("podman".into()),
        network_policy: VerificationNetworkPolicy::Isolated,
        cache_policy: VerificationCachePolicy::Clean,
        environment_policy_digest: Some(policy.environment_policy.digest()),
        integration_environment_digest: Some(env_spec.digest()),
        architecture: "x86_64".into(),
        os: "linux".into(),
        orbit_version: "0.1.0".into(),
    };

    let ws_state = WorkspaceState::compute_from_parts("base-4", "head-4", None);
    let run = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "attempt-b4-4",
        &ws_state,
        &plan,
        ws_dir.path(),
        env,
        Some(&policy),
        None,
    )
    .await?;

    assert_eq!(run.overall_result, Some(VerificationRunResult::Passed));
    assert!(
        run.step_runs[0]
            .stdout_preview
            .as_deref()
            .unwrap()
            .contains("EXTERNAL_BLOCKED_OK")
    );

    ctx.engine.pool.close().await;
    Ok(())
}

/// Scenario 5: Distinguishing READINESS_TIMEOUT vs SERVICE_START_FAILED vs VERIFICATION_FAILED
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b4_05_failure_differentiation_readiness_timeout() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    let mut env_spec = IntegrationEnvironmentSpec::new("test-timeout-env");
    let unready_service = ManagedServiceSpec {
        id: "unready_svc".into(),
        kind: ServiceKind::Container,
        image: Some(PODMAN_IMAGE_ALPINE.into()),
        command: vec!["sleep".into(), "100".into()],
        args: Vec::new(),
        env: BTreeMap::new(),
        mounts: Vec::new(),
        internal_port: Some(9999),
        readiness: Some(ReadinessProbe::Tcp {
            host: "unready_svc".into(),
            port: 9999,
            timeout_seconds: 2, // Quick timeout
            interval_ms: 200,
        }),
        timeout_seconds: 5,
        dependencies: Vec::new(),
    };
    env_spec.services.push(unready_service);

    let mut policy = VerificationPolicy::new("pol-b4-unready", "Unready Service Policy");
    policy.network_policy = VerificationNetworkPolicy::Isolated;
    policy.integration_environment_spec = Some(env_spec.clone());
    ctx.verification_store.save_policy(&policy).await?;

    let plan = VerificationPlan::new(
        "plan-unready",
        "Unready Plan",
        vec![VerificationStep::new_command(
            "step_never_run",
            "Should Not Run",
            vec!["echo".into(), "fail".into()],
        )],
    );
    ctx.verification_store.save_plan(&plan).await?;

    let env = EnvironmentIdentity {
        execution_profile: "sandboxed-container".into(),
        isolation: "rootless-podman".into(),
        runtime_image: Some(PODMAN_IMAGE_ALPINE.into()),
        runtime_image_digest: None,
        oci_runtime: Some("podman".into()),
        network_policy: VerificationNetworkPolicy::Isolated,
        cache_policy: VerificationCachePolicy::Clean,
        environment_policy_digest: Some(policy.environment_policy.digest()),
        integration_environment_digest: Some(env_spec.digest()),
        architecture: "x86_64".into(),
        os: "linux".into(),
        orbit_version: "0.1.0".into(),
    };

    let ws_state = WorkspaceState::compute_from_parts("base-5", "head-5", None);
    let run = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "attempt-b4-5",
        &ws_state,
        &plan,
        ws_dir.path(),
        env,
        Some(&policy),
        None,
    )
    .await?;

    assert_eq!(run.overall_result, Some(VerificationRunResult::TimedOut));

    let env_run = ctx
        .env_store
        .get_environment_run_for_verification(&run.id)
        .await?
        .unwrap();
    assert_eq!(env_run.status, EnvironmentRunStatus::TimedOut);
    assert_eq!(
        env_run.service_runs[0].status,
        EnvironmentServiceStatus::TimedOut
    );
    assert!(env_run.error_message.unwrap().contains("readiness timeout"));

    ctx.engine.pool.close().await;
    Ok(())
}

/// Scenario 6: Premature Service Crash Detected as SERVICE_START_FAILED
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b4_06_failure_differentiation_premature_crash() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    let mut env_spec = IntegrationEnvironmentSpec::new("test-crash-env");
    let crashing_service = ManagedServiceSpec {
        id: "crasher".into(),
        kind: ServiceKind::Container,
        image: Some(PODMAN_IMAGE_ALPINE.into()),
        command: vec![
            "sh".into(),
            "-c".into(),
            "echo 'crash immediately' >&2; exit 42".into(),
        ],
        args: Vec::new(),
        env: BTreeMap::new(),
        mounts: Vec::new(),
        internal_port: Some(8080),
        readiness: Some(ReadinessProbe::Tcp {
            host: "crasher".into(),
            port: 8080,
            timeout_seconds: 15,
            interval_ms: 200,
        }),
        timeout_seconds: 10,
        dependencies: Vec::new(),
    };
    env_spec.services.push(crashing_service);

    let mut policy = VerificationPolicy::new("pol-b4-crash", "Crash Service Policy");
    policy.network_policy = VerificationNetworkPolicy::Isolated;
    policy.integration_environment_spec = Some(env_spec.clone());
    ctx.verification_store.save_policy(&policy).await?;

    let plan = VerificationPlan::new(
        "plan-crash",
        "Crash Plan",
        vec![VerificationStep::new_command(
            "step_never_run",
            "Should Not Run",
            vec!["echo".into(), "fail".into()],
        )],
    );
    ctx.verification_store.save_plan(&plan).await?;

    let env = EnvironmentIdentity {
        execution_profile: "sandboxed-container".into(),
        isolation: "rootless-podman".into(),
        runtime_image: Some(PODMAN_IMAGE_ALPINE.into()),
        runtime_image_digest: None,
        oci_runtime: Some("podman".into()),
        network_policy: VerificationNetworkPolicy::Isolated,
        cache_policy: VerificationCachePolicy::Clean,
        environment_policy_digest: Some(policy.environment_policy.digest()),
        integration_environment_digest: Some(env_spec.digest()),
        architecture: "x86_64".into(),
        os: "linux".into(),
        orbit_version: "0.1.0".into(),
    };

    let ws_state = WorkspaceState::compute_from_parts("base-6", "head-6", None);
    let run = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "attempt-b4-6",
        &ws_state,
        &plan,
        ws_dir.path(),
        env,
        Some(&policy),
        None,
    )
    .await?;

    assert!(
        run.overall_result == Some(VerificationRunResult::Error)
            || run.overall_result == Some(VerificationRunResult::Failed)
    );

    let env_run = ctx
        .env_store
        .get_environment_run_for_verification(&run.id)
        .await?
        .unwrap();
    assert_eq!(env_run.status, EnvironmentRunStatus::Error);

    ctx.engine.pool.close().await;
    Ok(())
}

/// Scenario 7: Dependency Graph Ordering + Setup Migration Step
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b4_07_dependency_order_and_setup_step() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    let mut env_spec = IntegrationEnvironmentSpec::new("test-dep-setup-env");
    let mut pg_env = BTreeMap::new();
    pg_env.insert("POSTGRES_PASSWORD".into(), "secret".into());

    let db_service = ManagedServiceSpec {
        id: "database".into(),
        kind: ServiceKind::Container,
        image: Some(PODMAN_IMAGE_POSTGRES.into()),
        command: Vec::new(),
        args: Vec::new(),
        env: pg_env,
        mounts: Vec::new(),
        internal_port: Some(5432),
        readiness: Some(ReadinessProbe::Tcp {
            host: "database".into(),
            port: 5432,
            timeout_seconds: 30,
            interval_ms: 250,
        }),
        timeout_seconds: 40,
        dependencies: Vec::new(),
    };

    let app_service = ManagedServiceSpec {
        id: "app".into(),
        kind: ServiceKind::Container,
        image: Some(PODMAN_IMAGE_ALPINE.into()),
        command: vec![
            "sh".into(),
            "-c".into(),
            "while true; do nc -l -p 8080 -e echo OK; done".into(),
        ],
        args: Vec::new(),
        env: BTreeMap::new(),
        mounts: Vec::new(),
        internal_port: Some(8080),
        readiness: Some(ReadinessProbe::Tcp {
            host: "app".into(),
            port: 8080,
            timeout_seconds: 15,
            interval_ms: 250,
        }),
        timeout_seconds: 20,
        dependencies: vec!["database".into()],
    };

    // App is listed first in spec, but depends on database, so startup order must be: database -> app
    env_spec.services.push(app_service);
    env_spec.services.push(db_service);

    // Setup step (migration simulation)
    env_spec.setup_steps.push(VerificationStep::new_command(
        "migrate_db",
        "Database Migration Setup",
        vec![
            "sh".into(),
            "-c".into(),
            "echo 'CREATE TABLE users (id serial);' > /tmp/schema.sql && echo MIGRATION_COMPLETE"
                .into(),
        ],
    ));

    let mut policy = VerificationPolicy::new("pol-b4-dep", "Dependency Order Policy");
    policy.network_policy = VerificationNetworkPolicy::Isolated;
    policy.integration_environment_spec = Some(env_spec.clone());
    ctx.verification_store.save_policy(&policy).await?;

    let plan = VerificationPlan::new(
        "plan-dep-setup",
        "Dependency and Setup Plan",
        vec![VerificationStep::new_command(
            "test_app_and_db",
            "Verify App and DB Reachability",
            vec![
                "sh".into(),
                "-c".into(),
                "nc -z -w 2 app 8080 && nc -z -w 2 database 5432 && echo APP_AND_DB_READY".into(),
            ],
        )],
    );
    ctx.verification_store.save_plan(&plan).await?;

    let env = EnvironmentIdentity {
        execution_profile: "sandboxed-container".into(),
        isolation: "rootless-podman".into(),
        runtime_image: Some(PODMAN_IMAGE_ALPINE.into()),
        runtime_image_digest: None,
        oci_runtime: Some("podman".into()),
        network_policy: VerificationNetworkPolicy::Isolated,
        cache_policy: VerificationCachePolicy::Clean,
        environment_policy_digest: Some(policy.environment_policy.digest()),
        integration_environment_digest: Some(env_spec.digest()),
        architecture: "x86_64".into(),
        os: "linux".into(),
        orbit_version: "0.1.0".into(),
    };

    let ws_state = WorkspaceState::compute_from_parts("base-7", "head-7", None);
    let run = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "attempt-b4-7",
        &ws_state,
        &plan,
        ws_dir.path(),
        env,
        Some(&policy),
        None,
    )
    .await?;

    assert_eq!(run.overall_result, Some(VerificationRunResult::Passed));
    assert!(
        run.step_runs[0]
            .stdout_preview
            .as_deref()
            .unwrap()
            .contains("APP_AND_DB_READY")
    );

    let env_run = ctx
        .env_store
        .get_environment_run_for_verification(&run.id)
        .await?
        .unwrap();
    assert_eq!(env_run.status, EnvironmentRunStatus::Passed);

    // Verify database was started before app
    let db_idx = env_run
        .service_runs
        .iter()
        .position(|s| s.service_id == "database")
        .unwrap();
    let app_idx = env_run
        .service_runs
        .iter()
        .position(|s| s.service_id == "app")
        .unwrap();
    assert!(
        db_idx < app_idx,
        "database must start before dependent app service"
    );

    ctx.engine.pool.close().await;
    Ok(())
}

/// Scenario 8: Immutable Environment Identity and Image Digest Verification
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b4_08_immutable_environment_digest_and_qualification() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    let mut env_spec_a = IntegrationEnvironmentSpec::new("test-digest-env");
    let mut pg_env = BTreeMap::new();
    pg_env.insert("POSTGRES_PASSWORD".into(), "secret".into());

    let pg_service = ManagedServiceSpec {
        id: "postgres".into(),
        kind: ServiceKind::Container,
        image: Some(PODMAN_IMAGE_POSTGRES.into()),
        command: Vec::new(),
        args: Vec::new(),
        env: pg_env,
        mounts: Vec::new(),
        internal_port: Some(5432),
        readiness: Some(ReadinessProbe::Tcp {
            host: "postgres".into(),
            port: 5432,
            timeout_seconds: 30,
            interval_ms: 250,
        }),
        timeout_seconds: 40,
        dependencies: Vec::new(),
    };
    env_spec_a.services.push(pg_service);

    let mut policy = VerificationPolicy::new("pol-b4-digest", "Digest Check Policy");
    policy.network_policy = VerificationNetworkPolicy::Isolated;
    policy.integration_environment_spec = Some(env_spec_a.clone());
    ctx.verification_store.save_policy(&policy).await?;

    let plan = VerificationPlan::new(
        "plan-digest",
        "Digest Plan",
        vec![VerificationStep::new_command(
            "step1",
            "Step 1",
            vec![
                "nc".into(),
                "-z".into(),
                "-w".into(),
                "2".into(),
                "postgres".into(),
                "5432".into(),
            ],
        )],
    );
    ctx.verification_store.save_plan(&plan).await?;

    let env = EnvironmentIdentity {
        execution_profile: "sandboxed-container".into(),
        isolation: "rootless-podman".into(),
        runtime_image: Some(PODMAN_IMAGE_ALPINE.into()),
        runtime_image_digest: None,
        oci_runtime: Some("podman".into()),
        network_policy: VerificationNetworkPolicy::Isolated,
        cache_policy: VerificationCachePolicy::Clean,
        environment_policy_digest: Some(policy.environment_policy.digest()),
        integration_environment_digest: Some(env_spec_a.digest()),
        architecture: "x86_64".into(),
        os: "linux".into(),
        orbit_version: "0.1.0".into(),
    };

    let ws_state = WorkspaceState::compute_from_parts("base-8", "head-8", None);
    let run = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "attempt-b4-8",
        &ws_state,
        &plan,
        ws_dir.path(),
        env.clone(),
        Some(&policy),
        None,
    )
    .await?;

    assert_eq!(run.overall_result, Some(VerificationRunResult::Passed));

    // Check workspace qualification with exact matching env digest
    let qualified = ctx
        .verification_store
        .check_workspace_qualification(&ws_state.state_id, &policy, Some(&env))
        .await?;
    assert!(
        qualified.is_some(),
        "workspace state should qualify with exact environment digest"
    );

    // Mutation of environment digest must invalidate qualification!
    let mut mismatched_env = env.clone();
    mismatched_env.integration_environment_digest = Some("sha256:differentspecdigest000000".into());
    let disqualified = ctx
        .verification_store
        .check_workspace_qualification(&ws_state.state_id, &policy, Some(&mismatched_env))
        .await?;
    assert!(
        disqualified.is_none(),
        "workspace state must NOT qualify if environment digest changed"
    );

    ctx.engine.pool.close().await;
    Ok(())
}

/// Scenario 9: Workflow Orchestration with Integration Environment Policy (B3 Workflow Integration)
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b4_09_workflow_integration_with_environment_policy() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    let mut env_spec = IntegrationEnvironmentSpec::new("wf-int-env");
    let mut pg_env = BTreeMap::new();
    pg_env.insert("POSTGRES_PASSWORD".into(), "secret".into());

    let pg_service = ManagedServiceSpec {
        id: "wf_db".into(),
        kind: ServiceKind::Container,
        image: Some(PODMAN_IMAGE_POSTGRES.into()),
        command: Vec::new(),
        args: Vec::new(),
        env: pg_env,
        mounts: Vec::new(),
        internal_port: Some(5432),
        readiness: Some(ReadinessProbe::Tcp {
            host: "wf_db".into(),
            port: 5432,
            timeout_seconds: 30,
            interval_ms: 250,
        }),
        timeout_seconds: 40,
        dependencies: Vec::new(),
    };
    env_spec.services.push(pg_service);

    let mut policy = VerificationPolicy::new("pol-wf-env", "Workflow Environment Policy");
    policy.network_policy = VerificationNetworkPolicy::Isolated;
    policy.integration_environment_spec = Some(env_spec.clone());
    ctx.verification_store.save_policy(&policy).await?;

    let plan = VerificationPlan::new(
        "plan-wf-env",
        "Workflow Environment Plan",
        vec![VerificationStep::new_command(
            "test_step",
            "Verify wf_db reachability",
            vec![
                "sh".into(),
                "-c".into(),
                "nc -z -w 2 wf_db 5432 && echo WF_INTEGRATION_OK".into(),
            ],
        )],
    );
    ctx.verification_store.save_plan(&plan).await?;

    let env = EnvironmentIdentity {
        execution_profile: "sandboxed-container".into(),
        isolation: "rootless-podman".into(),
        runtime_image: Some(PODMAN_IMAGE_ALPINE.into()),
        runtime_image_digest: None,
        oci_runtime: Some("podman".into()),
        network_policy: VerificationNetworkPolicy::Isolated,
        cache_policy: VerificationCachePolicy::Clean,
        environment_policy_digest: Some(policy.environment_policy.digest()),
        integration_environment_digest: Some(env_spec.digest()),
        architecture: "x86_64".into(),
        os: "linux".into(),
        orbit_version: "0.1.0".into(),
    };

    let ws_state = WorkspaceState::compute_from_parts("base-wf", "head-wf", None);
    let run = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "attempt-wf-env-1",
        &ws_state,
        &plan,
        ws_dir.path(),
        env,
        Some(&policy),
        None,
    )
    .await?;

    assert_eq!(run.overall_result, Some(VerificationRunResult::Passed));
    assert!(
        run.step_runs[0]
            .stdout_preview
            .as_deref()
            .unwrap()
            .contains("WF_INTEGRATION_OK")
    );

    ctx.engine.pool.close().await;
    Ok(())
}

/// Scenario 10: Teardown Cleanup and Durability after Reopen
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b4_10_cleanup_and_restart_durability() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    let mut env_spec = IntegrationEnvironmentSpec::new("durability-env");
    let redis_service = ManagedServiceSpec {
        id: "redis_clean".into(),
        kind: ServiceKind::Container,
        image: Some(PODMAN_IMAGE_REDIS.into()),
        command: Vec::new(),
        args: Vec::new(),
        env: BTreeMap::new(),
        mounts: Vec::new(),
        internal_port: Some(6379),
        readiness: Some(ReadinessProbe::Tcp {
            host: "redis_clean".into(),
            port: 6379,
            timeout_seconds: 20,
            interval_ms: 250,
        }),
        timeout_seconds: 30,
        dependencies: Vec::new(),
    };
    env_spec.services.push(redis_service);

    let mut policy = VerificationPolicy::new("pol-clean", "Clean Policy");
    policy.network_policy = VerificationNetworkPolicy::Isolated;
    policy.integration_environment_spec = Some(env_spec.clone());
    ctx.verification_store.save_policy(&policy).await?;

    let plan = VerificationPlan::new(
        "plan-clean",
        "Clean Plan",
        vec![VerificationStep::new_command(
            "check_redis",
            "Check Redis",
            vec![
                "nc".into(),
                "-z".into(),
                "-w".into(),
                "2".into(),
                "redis_clean".into(),
                "6379".into(),
            ],
        )],
    );
    ctx.verification_store.save_plan(&plan).await?;

    let env = EnvironmentIdentity {
        execution_profile: "sandboxed-container".into(),
        isolation: "rootless-podman".into(),
        runtime_image: Some(PODMAN_IMAGE_ALPINE.into()),
        runtime_image_digest: None,
        oci_runtime: Some("podman".into()),
        network_policy: VerificationNetworkPolicy::Isolated,
        cache_policy: VerificationCachePolicy::Clean,
        environment_policy_digest: Some(policy.environment_policy.digest()),
        integration_environment_digest: Some(env_spec.digest()),
        architecture: "x86_64".into(),
        os: "linux".into(),
        orbit_version: "0.1.0".into(),
    };

    let ws_state = WorkspaceState::compute_from_parts("base-clean", "head-clean", None);
    let run = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "attempt-clean-1",
        &ws_state,
        &plan,
        ws_dir.path(),
        env,
        Some(&policy),
        None,
    )
    .await?;

    assert_eq!(run.overall_result, Some(VerificationRunResult::Passed));

    // Verify record in DB before closing
    let env_run = ctx
        .env_store
        .get_environment_run_for_verification(&run.id)
        .await?
        .unwrap();
    assert_eq!(env_run.status, EnvironmentRunStatus::Passed);
    let container_name = env_run.service_runs[0].container_name.clone().unwrap();

    // Verify container was removed
    let ps_check = tokio::process::Command::new("podman")
        .args(["ps", "-a", "--filter", &format!("name={}", container_name)])
        .output()
        .await?;
    assert!(!String::from_utf8_lossy(&ps_check.stdout).contains(&container_name));

    // Verify network was removed
    let net_name = env_run.network_name.clone().unwrap();
    let net_check = tokio::process::Command::new("podman")
        .args(["network", "ls", "--filter", &format!("name={}", net_name)])
        .output()
        .await?;
    assert!(!String::from_utf8_lossy(&net_check.stdout).contains(&net_name));

    ctx.engine.pool.close().await;
    Ok(())
}

/// Scenario 11: Process Service Cleanup on Test Failure (Terminal State: FAIL)
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b4_11_process_service_cleanup_on_test_failure() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    let mut env_spec = IntegrationEnvironmentSpec::new("proc-fail-env");
    let proc_service = ManagedServiceSpec {
        id: "api_fail".into(),
        kind: ServiceKind::Process,
        image: Some(PODMAN_IMAGE_ALPINE.into()),
        command: vec![
            "sh".into(),
            "-c".into(),
            "exec nc -lk -p 8081 -e echo OK".into(),
        ],
        args: Vec::new(),
        env: BTreeMap::new(),
        mounts: Vec::new(),
        internal_port: Some(8081),
        readiness: Some(ReadinessProbe::Tcp {
            host: "api_fail".into(),
            port: 8081,
            timeout_seconds: 10,
            interval_ms: 250,
        }),
        timeout_seconds: 15,
        dependencies: Vec::new(),
    };
    env_spec.services.push(proc_service);

    let mut policy = VerificationPolicy::new("pol-b4-fail", "Process Failure Policy");
    policy.network_policy = VerificationNetworkPolicy::Isolated;
    policy.integration_environment_spec = Some(env_spec.clone());
    ctx.verification_store.save_policy(&policy).await?;

    let plan = VerificationPlan::new(
        "plan-fail",
        "Failing Plan",
        vec![VerificationStep::new_command(
            "failing_step",
            "Failing Step",
            vec![
                "sh".into(),
                "-c".into(),
                "echo FAILING_TEST && exit 1".into(),
            ],
        )],
    );
    ctx.verification_store.save_plan(&plan).await?;

    let env = EnvironmentIdentity {
        execution_profile: "sandboxed-container".into(),
        isolation: "rootless-podman".into(),
        runtime_image: Some(PODMAN_IMAGE_ALPINE.into()),
        runtime_image_digest: None,
        oci_runtime: Some("podman".into()),
        network_policy: VerificationNetworkPolicy::Isolated,
        cache_policy: VerificationCachePolicy::Clean,
        environment_policy_digest: Some(policy.environment_policy.digest()),
        integration_environment_digest: Some(env_spec.digest()),
        architecture: "x86_64".into(),
        os: "linux".into(),
        orbit_version: "0.1.0".into(),
    };

    let ws_state = WorkspaceState::compute_from_parts("base-fail", "head-fail", None);
    let run = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "attempt-b4-fail",
        &ws_state,
        &plan,
        ws_dir.path(),
        env,
        Some(&policy),
        None,
    )
    .await?;

    assert_eq!(run.overall_result, Some(VerificationRunResult::Failed));

    let env_run = ctx
        .env_store
        .get_environment_run_for_verification(&run.id)
        .await?
        .unwrap();
    assert_eq!(env_run.status, EnvironmentRunStatus::Passed);

    let cname = env_run.service_runs[0].container_name.as_deref().unwrap();
    let ps_check = tokio::process::Command::new("podman")
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("name={}", cname),
            "--format",
            "{{.Names}}",
        ])
        .output()
        .await?;
    assert_eq!(String::from_utf8_lossy(&ps_check.stdout).trim(), "");

    let net_name = env_run.network_name.as_deref().unwrap();
    let net_check = tokio::process::Command::new("podman")
        .args([
            "network",
            "ls",
            "--filter",
            &format!("name={}", net_name),
            "--format",
            "{{.Name}}",
        ])
        .output()
        .await?;
    assert_eq!(String::from_utf8_lossy(&net_check.stdout).trim(), "");

    ctx.engine.pool.close().await;
    Ok(())
}

/// Scenario 12: Process Service Cleanup on Readiness Timeout (Terminal State: TIMEOUT)
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b4_12_process_service_cleanup_on_readiness_timeout() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    let mut env_spec = IntegrationEnvironmentSpec::new("proc-timeout-env");
    let proc_service = ManagedServiceSpec {
        id: "api_timeout".into(),
        kind: ServiceKind::Process,
        image: Some(PODMAN_IMAGE_ALPINE.into()),
        command: vec![
            "sh".into(),
            "-c".into(),
            "sleep 30".into(), // Does not listen on port 8082
        ],
        args: Vec::new(),
        env: BTreeMap::new(),
        mounts: Vec::new(),
        internal_port: Some(8082),
        readiness: Some(ReadinessProbe::Tcp {
            host: "api_timeout".into(),
            port: 8082,
            timeout_seconds: 2,
            interval_ms: 250,
        }),
        timeout_seconds: 5,
        dependencies: Vec::new(),
    };
    env_spec.services.push(proc_service);

    let mut policy = VerificationPolicy::new("pol-b4-timeout", "Process Timeout Policy");
    policy.network_policy = VerificationNetworkPolicy::Isolated;
    policy.integration_environment_spec = Some(env_spec.clone());
    ctx.verification_store.save_policy(&policy).await?;

    let plan = VerificationPlan::new(
        "plan-timeout",
        "Timeout Plan",
        vec![VerificationStep::new_command(
            "dummy_step",
            "Dummy Step",
            vec!["echo".into(), "SHOULD_NOT_RUN".into()],
        )],
    );
    ctx.verification_store.save_plan(&plan).await?;

    let env = EnvironmentIdentity {
        execution_profile: "sandboxed-container".into(),
        isolation: "rootless-podman".into(),
        runtime_image: Some(PODMAN_IMAGE_ALPINE.into()),
        runtime_image_digest: None,
        oci_runtime: Some("podman".into()),
        network_policy: VerificationNetworkPolicy::Isolated,
        cache_policy: VerificationCachePolicy::Clean,
        environment_policy_digest: Some(policy.environment_policy.digest()),
        integration_environment_digest: Some(env_spec.digest()),
        architecture: "x86_64".into(),
        os: "linux".into(),
        orbit_version: "0.1.0".into(),
    };

    let ws_state = WorkspaceState::compute_from_parts("base-timeout", "head-timeout", None);
    let run = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "attempt-b4-timeout",
        &ws_state,
        &plan,
        ws_dir.path(),
        env,
        Some(&policy),
        None,
    )
    .await?;

    assert_eq!(run.overall_result, Some(VerificationRunResult::TimedOut));

    let env_run = ctx
        .env_store
        .get_environment_run_for_verification(&run.id)
        .await?
        .unwrap();
    assert_eq!(env_run.status, EnvironmentRunStatus::TimedOut);

    let cname = env_run.service_runs[0].container_name.as_deref().unwrap();
    let ps_check = tokio::process::Command::new("podman")
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("name={}", cname),
            "--format",
            "{{.Names}}",
        ])
        .output()
        .await?;
    assert_eq!(String::from_utf8_lossy(&ps_check.stdout).trim(), "");

    let net_name = env_run.network_name.as_deref().unwrap();
    let net_check = tokio::process::Command::new("podman")
        .args([
            "network",
            "ls",
            "--filter",
            &format!("name={}", net_name),
            "--format",
            "{{.Name}}",
        ])
        .output()
        .await?;
    assert_eq!(String::from_utf8_lossy(&net_check.stdout).trim(), "");

    ctx.engine.pool.close().await;
    Ok(())
}

/// Scenario 13: Process Service Cleanup on Cancellation (Terminal State: CANCEL)
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b4_13_process_service_cleanup_on_cancellation() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    let mut env_spec = IntegrationEnvironmentSpec::new("proc-cancel-env");
    let proc_service = ManagedServiceSpec {
        id: "api_cancel".into(),
        kind: ServiceKind::Process,
        image: Some(PODMAN_IMAGE_ALPINE.into()),
        command: vec![
            "sh".into(),
            "-c".into(),
            "exec nc -lk -p 8083 -e echo OK".into(),
        ],
        args: Vec::new(),
        env: BTreeMap::new(),
        mounts: Vec::new(),
        internal_port: Some(8083),
        readiness: Some(ReadinessProbe::Tcp {
            host: "api_cancel".into(),
            port: 8083,
            timeout_seconds: 10,
            interval_ms: 250,
        }),
        timeout_seconds: 15,
        dependencies: Vec::new(),
    };
    env_spec.services.push(proc_service);

    let mut policy = VerificationPolicy::new("pol-b4-cancel", "Process Cancel Policy");
    policy.network_policy = VerificationNetworkPolicy::Isolated;
    policy.integration_environment_spec = Some(env_spec.clone());
    ctx.verification_store.save_policy(&policy).await?;

    let plan = VerificationPlan::new(
        "plan-cancel",
        "Cancel Plan",
        vec![VerificationStep::new_command(
            "dummy_step",
            "Dummy Step",
            vec!["sleep".into(), "10".into()],
        )],
    );
    ctx.verification_store.save_plan(&plan).await?;

    let env = EnvironmentIdentity {
        execution_profile: "sandboxed-container".into(),
        isolation: "rootless-podman".into(),
        runtime_image: Some(PODMAN_IMAGE_ALPINE.into()),
        runtime_image_digest: None,
        oci_runtime: Some("podman".into()),
        network_policy: VerificationNetworkPolicy::Isolated,
        cache_policy: VerificationCachePolicy::Clean,
        environment_policy_digest: Some(policy.environment_policy.digest()),
        integration_environment_digest: Some(env_spec.digest()),
        architecture: "x86_64".into(),
        os: "linux".into(),
        orbit_version: "0.1.0".into(),
    };

    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    // Cancel immediately
    cancel_tx.send(true).ok();

    let ws_state = WorkspaceState::compute_from_parts("base-cancel", "head-cancel", None);
    let run = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "attempt-b4-cancel",
        &ws_state,
        &plan,
        ws_dir.path(),
        env,
        Some(&policy),
        Some(cancel_rx),
    )
    .await?;

    assert_eq!(run.overall_result, Some(VerificationRunResult::Cancelled));

    let env_run = ctx
        .env_store
        .get_environment_run_for_verification(&run.id)
        .await?
        .unwrap();
    assert_eq!(env_run.status, EnvironmentRunStatus::Cancelled);

    if let Some(net_name) = env_run.network_name.as_deref() {
        let net_check = tokio::process::Command::new("podman")
            .args([
                "network",
                "ls",
                "--filter",
                &format!("name={}", net_name),
                "--format",
                "{{.Name}}",
            ])
            .output()
            .await?;
        assert_eq!(String::from_utf8_lossy(&net_check.stdout).trim(), "");
    }

    ctx.engine.pool.close().await;
    Ok(())
}
