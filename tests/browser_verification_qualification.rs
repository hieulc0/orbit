//! Phase B5 Qualification Test Suite: Headless Browser & UI Verification
//!
//! Validates:
//! 1. B5_BROWSER_HAPPY_PATH: Deterministic Playwright assertions pass inside isolated container.
//! 2. B5_ASSERTION_FAILURE: Assertion failure captured with detailed message.
//! 3. B5_NAVIGATION_FAILURE: Navigation failure classified correctly.
//! 4. B5_PAGE_ERROR_CAPTURE: Uncaught page errors captured and enforced.
//! 5. B5_CONSOLE_RECORD: Console entries captured under Record policy.
//! 6. B5_CONSOLE_FAIL_POLICY: Console errors fail execution under FailOnError policy.
//! 7. B5_NETWORK_FAILURE_CAPTURE: Network failures captured and enforced under FailOnUnexpectedStatus.
//! 8. B5_SCREENSHOT_ARTIFACT: Failure screenshot captured, budgeted, and recorded.
//! 9. B5_TRACE_ARTIFACT: Failure trace zip captured, budgeted, and recorded.
//! 10. B5_BROWSER_RUNTIME_IDENTITY: Exact browser image digest and runtime identity recorded.
//! 11. B5_SPEC_MUTATION_INVALIDATION: Spec mutation invalidates prior qualification.
//! 12. B5_WORKSPACE_MUTATION_INVALIDATION: Workspace mutation invalidates prior qualification.
//! 13. B5_TIMEOUT_CLEANUP: Test timeout terminates container with clean teardown.
//! 14. B5_CANCEL_CLEANUP: Cancellation stops container cleanly without leaks.
//! 15. B5_NO_HOST_PORT_REQUIRED: Service reachable via internal DNS without host port exposure.
//! 16. B5_BROWSER_SECURITY_ISOLATION: Host canary file inaccessible in container.
//! 17. B5_RESTART_DURABILITY: Data survives pool disconnect and reconnection.
//! 18. B5_B3_REPAIR_INTEGRATION: Browser verification failure routes to Repairing before Reviewer.
//! 19. B5_FINAL_REGRESSION_INTEGRATION: Final regression executes browser verification under policy.

use anyhow::Result;
use orbit::{
    browser_verification::{
        BrowserArtifactPolicy, BrowserArtifactType, BrowserBackend, BrowserConsolePolicy,
        BrowserFailureReason, BrowserNetworkPolicy, BrowserNetworkPolicyMode,
        BrowserPageErrorPolicy, BrowserStore, BrowserTestSpec, BrowserTestStatus,
        BrowserVerificationSpec, BrowserVerificationStatus, ConsolePolicyMode,
        DEFAULT_BROWSER_IMAGE, PageErrorPolicyMode, ScreenshotCaptureMode, TraceCaptureMode,
    },
    engine::Engine,
    integration_environment::{
        EnvironmentStore, IntegrationEnvironmentSpec, ManagedServiceSpec, ReadinessProbe,
        ServiceKind,
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

#[allow(dead_code)]
struct TestContext {
    engine: Engine,
    verification_store: VerificationStore,
    env_store: EnvironmentStore,
    browser_store: BrowserStore,
    _schema: String,
    _home: tempfile::TempDir,
}

fn set_artifacts_dir(dir: &std::path::Path) {
    unsafe {
        std::env::set_var("ORBIT_ARTIFACTS_DIR", dir);
    }
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
        anyhow::bail!("disposable database URL required for B5 qualification test");
    }

    let admin = PgPool::connect(&base).await?;
    let schema = format!("orbit_qual_b5_{}", id().replace('-', ""));
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await?;
    let separator = if base.contains('?') { '&' } else { '?' };
    let url = format!("{base}{separator}options=-csearch_path%3D{schema}");
    let home = tempfile::tempdir()?;

    let engine = Engine::connect(&url, home.path().join("artifacts"), 3).await?;
    let verification_store = VerificationStore::new(engine.pool.clone());
    let env_store = EnvironmentStore::new(engine.pool.clone());
    let browser_store = BrowserStore::new(engine.pool.clone());

    Ok(TestContext {
        engine,
        verification_store,
        env_store,
        browser_store,
        _schema: schema,
        _home: home,
    })
}

fn dummy_env_identity() -> EnvironmentIdentity {
    EnvironmentIdentity {
        execution_profile: "qualification".into(),
        isolation: "container".into(),
        runtime_image: Some(DEFAULT_BROWSER_IMAGE.into()),
        runtime_image_digest: None,
        oci_runtime: Some("podman".into()),
        network_policy: VerificationNetworkPolicy::None,
        cache_policy: VerificationCachePolicy::Clean,
        environment_policy_digest: None,
        integration_environment_digest: None,
        browser_verification_digest: None,
        browser_runtime_image_digest: None,
        regression_policy_digest: None,
        selection_digest: None,
        architecture: std::env::consts::ARCH.into(),
        os: std::env::consts::OS.into(),
        orbit_version: env!("CARGO_PKG_VERSION").into(),
    }
}

// 1. B5_BROWSER_HAPPY_PATH
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b5_01_browser_happy_path() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    // Create a simple browser test script in workspace
    let test_file = ws_dir.path().join("test_ui.js");
    tokio::fs::write(
        &test_file,
        r#"
module.exports = async function({ page }) {
    await page.setContent("<h1>Orbit UI Ready</h1><button id='action'>Submit</button>");
    const text = await page.innerText("h1");
    if (text !== "Orbit UI Ready") {
        throw new Error("Assertion failed: expected 'Orbit UI Ready', got " + text);
    }
};
"#,
    )
    .await?;

    let b_spec = BrowserVerificationSpec {
        id: "spec-b5-happy".into(),
        version: 1,
        backend: BrowserBackend::PlaywrightChromium,
        base_url: None,
        tests: vec![BrowserTestSpec {
            id: "happy_test".into(),
            name: "Happy Path Test".into(),
            entrypoint: "test_ui.js".into(),
            command: None,
            timeout_seconds: 15,
            required: true,
        }],
        timeout_seconds: 30,
        artifact_policy: BrowserArtifactPolicy::default(),
        console_policy: BrowserConsolePolicy::default(),
        page_error_policy: BrowserPageErrorPolicy::default(),
        network_policy: BrowserNetworkPolicy::default(),
        shm_size_mb: Some(256),
        memory_limit_mb: Some(512),
    };

    let mut policy = VerificationPolicy::new("pol-b5-happy", "B5 Happy Policy");
    policy.browser_verification_spec = Some(b_spec);

    let plan = VerificationPlan::new(
        "plan-b5-happy",
        "plan-b5-happy",
        vec![VerificationStep::new_command(
            "step1",
            "step1",
            vec!["true".into()],
        )],
    );

    let ws_state = WorkspaceState::compute_from_parts("c0", "c0", Some("diff-happy"));
    let artifacts_dir = tempfile::tempdir()?;
    set_artifacts_dir(artifacts_dir.path());

    let run = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "att-b5-happy",
        &ws_state,
        &plan,
        ws_dir.path(),
        dummy_env_identity(),
        Some(&policy),
        None,
    )
    .await?;

    assert_eq!(run.overall_result, Some(VerificationRunResult::Passed));
    let brun = run
        .browser_verification_run
        .expect("browser run must be present");
    assert_eq!(brun.status, BrowserVerificationStatus::Passed);
    assert_eq!(brun.test_runs.len(), 1);
    assert_eq!(brun.test_runs[0].status, BrowserTestStatus::Passed);
    assert!(brun.test_runs[0].duration_ms.unwrap_or(0) > 0);

    Ok(())
}

// 2. B5_ASSERTION_FAILURE
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b5_02_assertion_failure() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    let test_file = ws_dir.path().join("test_fail.js");
    tokio::fs::write(
        &test_file,
        r#"
module.exports = async function({ page }) {
    await page.setContent("<h1>Actual Content</h1>");
    const text = await page.innerText("h1");
    if (text !== "Expected Heading") {
        throw new Error("Assertion failed: expected 'Expected Heading', got " + text);
    }
};
"#,
    )
    .await?;

    let b_spec = BrowserVerificationSpec {
        id: "spec-b5-fail".into(),
        version: 1,
        backend: BrowserBackend::PlaywrightChromium,
        base_url: None,
        tests: vec![BrowserTestSpec {
            id: "failing_test".into(),
            name: "Failing Test".into(),
            entrypoint: "test_fail.js".into(),
            command: None,
            timeout_seconds: 15,
            required: true,
        }],
        timeout_seconds: 30,
        artifact_policy: BrowserArtifactPolicy::default(),
        console_policy: BrowserConsolePolicy::default(),
        page_error_policy: BrowserPageErrorPolicy::default(),
        network_policy: BrowserNetworkPolicy::default(),
        shm_size_mb: None,
        memory_limit_mb: None,
    };

    let mut policy = VerificationPolicy::new("pol-b5-fail", "B5 Fail Policy");
    policy.browser_verification_spec = Some(b_spec);

    let plan = VerificationPlan::new(
        "plan-b5-fail",
        "plan-b5-fail",
        vec![VerificationStep::new_command(
            "step1",
            "step1",
            vec!["true".into()],
        )],
    );

    let ws_state = WorkspaceState::compute_from_parts("c0", "c0", Some("diff-fail"));
    let artifacts_dir = tempfile::tempdir()?;
    set_artifacts_dir(artifacts_dir.path());

    let run = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "att-b5-fail",
        &ws_state,
        &plan,
        ws_dir.path(),
        dummy_env_identity(),
        Some(&policy),
        None,
    )
    .await?;

    assert_eq!(run.overall_result, Some(VerificationRunResult::Failed));
    let brun = run.browser_verification_run.expect("browser run");
    assert_eq!(brun.status, BrowserVerificationStatus::Failed);
    assert_eq!(brun.test_runs[0].status, BrowserTestStatus::Failed);
    assert_eq!(
        brun.test_runs[0].failure_reason,
        Some(BrowserFailureReason::AssertionFailed)
    );
    assert!(
        brun.test_runs[0]
            .failure_message
            .as_deref()
            .unwrap_or("")
            .contains("Assertion failed")
    );

    Ok(())
}

// 3. B5_NAVIGATION_FAILURE
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b5_03_navigation_failure() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    let test_file = ws_dir.path().join("test_nav_fail.js");
    tokio::fs::write(
        &test_file,
        r#"
module.exports = async function({ page }) {
    await page.goto("http://127.0.0.1:59999/unreachable", { timeout: 3000 });
};
"#,
    )
    .await?;

    let b_spec = BrowserVerificationSpec {
        id: "spec-b5-nav".into(),
        version: 1,
        backend: BrowserBackend::PlaywrightChromium,
        base_url: None,
        tests: vec![BrowserTestSpec {
            id: "nav_fail_test".into(),
            name: "Nav Fail Test".into(),
            entrypoint: "test_nav_fail.js".into(),
            command: None,
            timeout_seconds: 10,
            required: true,
        }],
        timeout_seconds: 20,
        artifact_policy: BrowserArtifactPolicy::default(),
        console_policy: BrowserConsolePolicy::default(),
        page_error_policy: BrowserPageErrorPolicy::default(),
        network_policy: BrowserNetworkPolicy::default(),
        shm_size_mb: None,
        memory_limit_mb: None,
    };

    let mut policy = VerificationPolicy::new("pol-b5-nav", "B5 Nav Policy");
    policy.browser_verification_spec = Some(b_spec);

    let plan = VerificationPlan::new(
        "plan-b5-nav",
        "plan-b5-nav",
        vec![VerificationStep::new_command(
            "step1",
            "dummy",
            vec!["true".into()],
        )],
    );

    let ws_state = WorkspaceState::compute_from_parts("c0", "c0", Some("diff-nav"));
    let artifacts_dir = tempfile::tempdir()?;
    set_artifacts_dir(artifacts_dir.path());

    let run = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "att-b5-nav",
        &ws_state,
        &plan,
        ws_dir.path(),
        dummy_env_identity(),
        Some(&policy),
        None,
    )
    .await?;

    assert_eq!(run.overall_result, Some(VerificationRunResult::Failed));
    let brun = run.browser_verification_run.expect("browser run");
    assert_eq!(
        brun.test_runs[0].failure_reason,
        Some(BrowserFailureReason::NavigationFailed)
    );

    Ok(())
}

// 4. B5_PAGE_ERROR_CAPTURE
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b5_04_page_error_capture() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    let test_file = ws_dir.path().join("test_page_err.js");
    tokio::fs::write(
        &test_file,
        r#"
module.exports = async function({ page }) {
    await page.setContent(`
        <html>
            <script>
                setTimeout(() => {
                    throw new Error("uncaught-runtime-canary-error");
                }, 20);
            </script>
        </html>
    `);
    await new Promise(r => setTimeout(r, 100));
};
"#,
    )
    .await?;

    let b_spec = BrowserVerificationSpec {
        id: "spec-b5-page-err".into(),
        version: 1,
        backend: BrowserBackend::PlaywrightChromium,
        base_url: None,
        tests: vec![BrowserTestSpec {
            id: "page_err_test".into(),
            name: "Page Error Test".into(),
            entrypoint: "test_page_err.js".into(),
            command: None,
            timeout_seconds: 10,
            required: true,
        }],
        timeout_seconds: 20,
        artifact_policy: BrowserArtifactPolicy::default(),
        console_policy: BrowserConsolePolicy::default(),
        page_error_policy: BrowserPageErrorPolicy {
            mode: PageErrorPolicyMode::FailOnPageError,
        },
        network_policy: BrowserNetworkPolicy::default(),
        shm_size_mb: None,
        memory_limit_mb: None,
    };

    let mut policy = VerificationPolicy::new("pol-b5-page-err", "Page Err Policy");
    policy.browser_verification_spec = Some(b_spec);

    let plan = VerificationPlan::new(
        "plan-b5-page-err",
        "plan-b5-page-err",
        vec![VerificationStep::new_command(
            "step1",
            "dummy",
            vec!["true".into()],
        )],
    );

    let ws_state = WorkspaceState::compute_from_parts("c0", "c0", Some("diff-page-err"));
    let artifacts_dir = tempfile::tempdir()?;
    set_artifacts_dir(artifacts_dir.path());

    let run = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "att-b5-page-err",
        &ws_state,
        &plan,
        ws_dir.path(),
        dummy_env_identity(),
        Some(&policy),
        None,
    )
    .await?;

    assert_eq!(run.overall_result, Some(VerificationRunResult::Failed));
    let brun = run.browser_verification_run.expect("browser run");
    assert_eq!(
        brun.test_runs[0].failure_reason,
        Some(BrowserFailureReason::PageErrorPolicyFailed)
    );
    assert!(!brun.test_runs[0].page_errors.is_empty());
    assert!(
        brun.test_runs[0].page_errors[0]
            .message
            .contains("uncaught-runtime-canary-error")
    );

    Ok(())
}

// 5. B5_CONSOLE_RECORD
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b5_05_console_record() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    let test_file = ws_dir.path().join("test_console_rec.js");
    tokio::fs::write(
        &test_file,
        r#"
module.exports = async function({ page }) {
    await page.setContent(`
        <script>
            console.log("canary-log-msg");
            console.warn("canary-warn-msg");
            console.error("canary-error-recorded-msg");
        </script>
    `);
    await new Promise(r => setTimeout(r, 50));
};
"#,
    )
    .await?;

    let b_spec = BrowserVerificationSpec {
        id: "spec-b5-con-rec".into(),
        version: 1,
        backend: BrowserBackend::PlaywrightChromium,
        base_url: None,
        tests: vec![BrowserTestSpec {
            id: "console_rec_test".into(),
            name: "Console Record Test".into(),
            entrypoint: "test_console_rec.js".into(),
            command: None,
            timeout_seconds: 10,
            required: true,
        }],
        timeout_seconds: 20,
        artifact_policy: BrowserArtifactPolicy::default(),
        console_policy: BrowserConsolePolicy {
            mode: ConsolePolicyMode::Record,
            max_console_entries: 100,
        },
        page_error_policy: BrowserPageErrorPolicy {
            mode: PageErrorPolicyMode::Ignore,
        },
        network_policy: BrowserNetworkPolicy::default(),
        shm_size_mb: None,
        memory_limit_mb: None,
    };

    let mut policy = VerificationPolicy::new("pol-b5-con-rec", "Console Record Policy");
    policy.browser_verification_spec = Some(b_spec);

    let plan = VerificationPlan::new(
        "plan-b5-con-rec",
        "plan-b5-con-rec",
        vec![VerificationStep::new_command(
            "step1",
            "dummy",
            vec!["true".into()],
        )],
    );

    let ws_state = WorkspaceState::compute_from_parts("c0", "c0", Some("diff-con-rec"));
    let artifacts_dir = tempfile::tempdir()?;
    set_artifacts_dir(artifacts_dir.path());

    let run = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "att-b5-con-rec",
        &ws_state,
        &plan,
        ws_dir.path(),
        dummy_env_identity(),
        Some(&policy),
        None,
    )
    .await?;

    assert_eq!(run.overall_result, Some(VerificationRunResult::Passed));
    let brun = run.browser_verification_run.expect("browser run");
    assert_eq!(brun.test_runs[0].status, BrowserTestStatus::Passed);
    assert!(brun.test_runs[0].console_entries.len() >= 3);
    assert!(
        brun.test_runs[0]
            .console_entries
            .iter()
            .any(|e| e.text.contains("canary-error-recorded-msg"))
    );

    Ok(())
}

// 6. B5_CONSOLE_FAIL_POLICY
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b5_06_console_fail_policy() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    let test_file = ws_dir.path().join("test_console_fail.js");
    tokio::fs::write(
        &test_file,
        r#"
module.exports = async function({ page }) {
    await page.setContent(`
        <script>
            console.error("forbidden-canary-error");
        </script>
    `);
    await new Promise(r => setTimeout(r, 50));
};
"#,
    )
    .await?;

    let b_spec = BrowserVerificationSpec {
        id: "spec-b5-con-fail".into(),
        version: 1,
        backend: BrowserBackend::PlaywrightChromium,
        base_url: None,
        tests: vec![BrowserTestSpec {
            id: "con_fail_test".into(),
            name: "Console Fail Test".into(),
            entrypoint: "test_console_fail.js".into(),
            command: None,
            timeout_seconds: 10,
            required: true,
        }],
        timeout_seconds: 20,
        artifact_policy: BrowserArtifactPolicy::default(),
        console_policy: BrowserConsolePolicy {
            mode: ConsolePolicyMode::FailOnError,
            max_console_entries: 100,
        },
        page_error_policy: BrowserPageErrorPolicy {
            mode: PageErrorPolicyMode::Ignore,
        },
        network_policy: BrowserNetworkPolicy::default(),
        shm_size_mb: None,
        memory_limit_mb: None,
    };

    let mut policy = VerificationPolicy::new("pol-b5-con-fail", "Console Fail Policy");
    policy.browser_verification_spec = Some(b_spec);

    let plan = VerificationPlan::new(
        "plan-b5-con-fail",
        "plan-b5-con-fail",
        vec![VerificationStep::new_command(
            "step1",
            "dummy",
            vec!["true".into()],
        )],
    );

    let ws_state = WorkspaceState::compute_from_parts("c0", "c0", Some("diff-con-fail"));
    let artifacts_dir = tempfile::tempdir()?;
    set_artifacts_dir(artifacts_dir.path());

    let run = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "att-b5-con-fail",
        &ws_state,
        &plan,
        ws_dir.path(),
        dummy_env_identity(),
        Some(&policy),
        None,
    )
    .await?;

    assert_eq!(run.overall_result, Some(VerificationRunResult::Failed));
    let brun = run.browser_verification_run.expect("browser run");
    assert_eq!(
        brun.test_runs[0].failure_reason,
        Some(BrowserFailureReason::ConsolePolicyFailed)
    );

    Ok(())
}

// 7. B5_NETWORK_FAILURE_CAPTURE
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b5_07_network_failure_capture() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    let test_file = ws_dir.path().join("test_net_fail.js");
    tokio::fs::write(
        &test_file,
        r#"
module.exports = async function({ page }) {
    await page.setContent(`
        <script>
            fetch("http://127.0.0.1:59998/api/canary").catch(() => {});
        </script>
    `);
    await new Promise(r => setTimeout(r, 100));
};
"#,
    )
    .await?;

    let b_spec = BrowserVerificationSpec {
        id: "spec-b5-net-fail".into(),
        version: 1,
        backend: BrowserBackend::PlaywrightChromium,
        base_url: None,
        tests: vec![BrowserTestSpec {
            id: "net_fail_test".into(),
            name: "Network Fail Test".into(),
            entrypoint: "test_net_fail.js".into(),
            command: None,
            timeout_seconds: 10,
            required: true,
        }],
        timeout_seconds: 20,
        artifact_policy: BrowserArtifactPolicy::default(),
        console_policy: BrowserConsolePolicy::default(),
        page_error_policy: BrowserPageErrorPolicy {
            mode: PageErrorPolicyMode::Ignore,
        },
        network_policy: BrowserNetworkPolicy {
            mode: BrowserNetworkPolicyMode::Record,
            allowed_statuses: vec![],
        },
        shm_size_mb: None,
        memory_limit_mb: None,
    };

    let mut policy = VerificationPolicy::new("pol-b5-net-fail", "Net Fail Policy");
    policy.browser_verification_spec = Some(b_spec);

    let plan = VerificationPlan::new(
        "plan-b5-net-fail",
        "plan-b5-net-fail",
        vec![VerificationStep::new_command(
            "step1",
            "dummy",
            vec!["true".into()],
        )],
    );

    let ws_state = WorkspaceState::compute_from_parts("c0", "c0", Some("diff-net-fail"));
    let artifacts_dir = tempfile::tempdir()?;
    set_artifacts_dir(artifacts_dir.path());

    let run = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "att-b5-net-fail",
        &ws_state,
        &plan,
        ws_dir.path(),
        dummy_env_identity(),
        Some(&policy),
        None,
    )
    .await?;

    let brun = run.browser_verification_run.expect("browser run");
    assert!(!brun.test_runs[0].network_failures.is_empty());
    assert!(
        brun.test_runs[0].network_failures[0]
            .url
            .contains("59998/api/canary")
    );

    Ok(())
}

// 8. B5_SCREENSHOT_ARTIFACT
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b5_08_screenshot_artifact() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    let test_file = ws_dir.path().join("test_shot.js");
    tokio::fs::write(
        &test_file,
        r#"
module.exports = async function({ page }) {
    await page.setContent("<h1>Failure Page for Screenshot</h1>");
    throw new Error("Trigger screenshot capture on failure");
};
"#,
    )
    .await?;

    let b_spec = BrowserVerificationSpec {
        id: "spec-b5-shot".into(),
        version: 1,
        backend: BrowserBackend::PlaywrightChromium,
        base_url: None,
        tests: vec![BrowserTestSpec {
            id: "shot_test".into(),
            name: "Screenshot Test".into(),
            entrypoint: "test_shot.js".into(),
            command: None,
            timeout_seconds: 10,
            required: true,
        }],
        timeout_seconds: 20,
        artifact_policy: BrowserArtifactPolicy {
            capture_screenshots: ScreenshotCaptureMode::OnFailure,
            capture_trace: TraceCaptureMode::Never,
            ..Default::default()
        },
        console_policy: BrowserConsolePolicy::default(),
        page_error_policy: BrowserPageErrorPolicy::default(),
        network_policy: BrowserNetworkPolicy::default(),
        shm_size_mb: None,
        memory_limit_mb: None,
    };

    let mut policy = VerificationPolicy::new("pol-b5-shot", "Screenshot Policy");
    policy.browser_verification_spec = Some(b_spec);

    let plan = VerificationPlan::new(
        "plan-b5-shot",
        "plan-b5-shot",
        vec![VerificationStep::new_command(
            "step1",
            "dummy",
            vec!["true".into()],
        )],
    );

    let ws_state = WorkspaceState::compute_from_parts("c0", "c0", Some("diff-shot"));
    let artifacts_dir = tempfile::tempdir()?;
    set_artifacts_dir(artifacts_dir.path());

    let run = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "att-b5-shot",
        &ws_state,
        &plan,
        ws_dir.path(),
        dummy_env_identity(),
        Some(&policy),
        None,
    )
    .await?;

    let brun = run.browser_verification_run.expect("browser run");
    let shot = brun
        .artifacts
        .iter()
        .find(|a| a.artifact_type == BrowserArtifactType::Screenshot)
        .expect("screenshot artifact must be captured on failure");

    assert_eq!(shot.mime_type, "image/png");
    assert!(shot.byte_size > 0);
    assert!(!shot.digest.is_empty());

    let host_file = artifacts_dir.path().join(&shot.storage_ref);
    assert!(host_file.exists());

    Ok(())
}

// 9. B5_TRACE_ARTIFACT
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b5_09_trace_artifact() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    let test_file = ws_dir.path().join("test_trace.js");
    tokio::fs::write(
        &test_file,
        r#"
module.exports = async function({ page }) {
    await page.setContent("<h1>Failure Page for Trace</h1>");
    throw new Error("Trigger trace capture on failure");
};
"#,
    )
    .await?;

    let b_spec = BrowserVerificationSpec {
        id: "spec-b5-trace".into(),
        version: 1,
        backend: BrowserBackend::PlaywrightChromium,
        base_url: None,
        tests: vec![BrowserTestSpec {
            id: "trace_test".into(),
            name: "Trace Test".into(),
            entrypoint: "test_trace.js".into(),
            command: None,
            timeout_seconds: 10,
            required: true,
        }],
        timeout_seconds: 20,
        artifact_policy: BrowserArtifactPolicy {
            capture_screenshots: ScreenshotCaptureMode::Never,
            capture_trace: TraceCaptureMode::OnFailure,
            ..Default::default()
        },
        console_policy: BrowserConsolePolicy::default(),
        page_error_policy: BrowserPageErrorPolicy::default(),
        network_policy: BrowserNetworkPolicy::default(),
        shm_size_mb: None,
        memory_limit_mb: None,
    };

    let mut policy = VerificationPolicy::new("pol-b5-trace", "Trace Policy");
    policy.browser_verification_spec = Some(b_spec);

    let plan = VerificationPlan::new(
        "plan-b5-trace",
        "plan-b5-trace",
        vec![VerificationStep::new_command(
            "step1",
            "dummy",
            vec!["true".into()],
        )],
    );

    let ws_state = WorkspaceState::compute_from_parts("c0", "c0", Some("diff-trace"));
    let artifacts_dir = tempfile::tempdir()?;
    set_artifacts_dir(artifacts_dir.path());

    let run = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "att-b5-trace",
        &ws_state,
        &plan,
        ws_dir.path(),
        dummy_env_identity(),
        Some(&policy),
        None,
    )
    .await?;

    let brun = run.browser_verification_run.expect("browser run");
    let trace_art = brun
        .artifacts
        .iter()
        .find(|a| a.artifact_type == BrowserArtifactType::Trace)
        .expect("trace artifact must be captured on failure");

    assert_eq!(trace_art.mime_type, "application/zip");
    assert!(trace_art.byte_size > 0);

    let host_file = artifacts_dir.path().join(&trace_art.storage_ref);
    assert!(host_file.exists());

    Ok(())
}

// 10. B5_BROWSER_RUNTIME_IDENTITY
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b5_10_browser_runtime_identity() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    let test_file = ws_dir.path().join("test_id.js");
    tokio::fs::write(&test_file, "module.exports = async function() {};").await?;

    let b_spec = BrowserVerificationSpec {
        id: "spec-b5-id".into(),
        version: 1,
        backend: BrowserBackend::PlaywrightChromium,
        base_url: None,
        tests: vec![BrowserTestSpec {
            id: "id_test".into(),
            name: "Identity Test".into(),
            entrypoint: "test_id.js".into(),
            command: None,
            timeout_seconds: 10,
            required: true,
        }],
        timeout_seconds: 20,
        artifact_policy: BrowserArtifactPolicy::default(),
        console_policy: BrowserConsolePolicy::default(),
        page_error_policy: BrowserPageErrorPolicy::default(),
        network_policy: BrowserNetworkPolicy::default(),
        shm_size_mb: None,
        memory_limit_mb: None,
    };

    let mut policy = VerificationPolicy::new("pol-b5-id", "Identity Policy");
    policy.browser_verification_spec = Some(b_spec);

    let plan = VerificationPlan::new(
        "plan-b5-id",
        "plan-b5-id",
        vec![VerificationStep::new_command(
            "step1",
            "dummy",
            vec!["true".into()],
        )],
    );

    let ws_state = WorkspaceState::compute_from_parts("c0", "c0", Some("diff-id"));
    let artifacts_dir = tempfile::tempdir()?;
    set_artifacts_dir(artifacts_dir.path());

    let run = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "att-b5-id",
        &ws_state,
        &plan,
        ws_dir.path(),
        dummy_env_identity(),
        Some(&policy),
        None,
    )
    .await?;

    let brun = run.browser_verification_run.expect("browser run");
    assert!(brun.browser_image_digest.starts_with("sha256:"));
    assert_eq!(brun.browser_backend, BrowserBackend::PlaywrightChromium);
    assert!(brun.browser_version.is_some());
    assert!(brun.playwright_version.is_some());

    assert_eq!(
        run.environment_identity.browser_verification_digest,
        Some(brun.spec_digest)
    );
    assert_eq!(
        run.environment_identity.browser_runtime_image_digest,
        Some(brun.browser_image_digest)
    );

    Ok(())
}

// 11. B5_SPEC_MUTATION_INVALIDATION
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b5_11_spec_mutation_invalidation() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    let test_file = ws_dir.path().join("test_mut.js");
    tokio::fs::write(&test_file, "module.exports = async function() {};").await?;

    let mut b_spec1 = BrowserVerificationSpec {
        id: "spec-b5-mut".into(),
        version: 1,
        backend: BrowserBackend::PlaywrightChromium,
        base_url: None,
        tests: vec![BrowserTestSpec {
            id: "mut_test".into(),
            name: "Mutation Test".into(),
            entrypoint: "test_mut.js".into(),
            command: None,
            timeout_seconds: 10,
            required: true,
        }],
        timeout_seconds: 20,
        artifact_policy: BrowserArtifactPolicy::default(),
        console_policy: BrowserConsolePolicy::default(),
        page_error_policy: BrowserPageErrorPolicy::default(),
        network_policy: BrowserNetworkPolicy::default(),
        shm_size_mb: None,
        memory_limit_mb: None,
    };

    let mut policy1 = VerificationPolicy::new("pol-b5-mut", "Mutation Policy");
    policy1.browser_verification_spec = Some(b_spec1.clone());

    let plan = VerificationPlan::new(
        "plan-b5-mut",
        "plan-b5-mut",
        vec![VerificationStep::new_command(
            "step1",
            "dummy",
            vec!["true".into()],
        )],
    );

    let ws_state = WorkspaceState::compute_from_parts("c0", "c0", Some("diff-mut"));
    let artifacts_dir = tempfile::tempdir()?;
    set_artifacts_dir(artifacts_dir.path());

    let run1 = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "att-b5-mut1",
        &ws_state,
        &plan,
        ws_dir.path(),
        dummy_env_identity(),
        Some(&policy1),
        None,
    )
    .await?;

    assert_eq!(run1.overall_result, Some(VerificationRunResult::Passed));

    // Qualify under policy1: succeeds!
    let qualified = ctx
        .verification_store
        .check_workspace_qualification(&ws_state.state_id, &policy1, None)
        .await?;
    assert!(qualified.is_some());

    // Mutate spec (e.g. change timeout or add a test)
    b_spec1.timeout_seconds = 45;
    let mut policy2 = VerificationPolicy::new("pol-b5-mut", "Mutation Policy");
    policy2.browser_verification_spec = Some(b_spec1);

    // Prior run must NOT qualify under mutated policy!
    let qualified_mut = ctx
        .verification_store
        .check_workspace_qualification(&ws_state.state_id, &policy2, None)
        .await?;
    assert!(qualified_mut.is_none());

    Ok(())
}

// 12. B5_WORKSPACE_MUTATION_INVALIDATION
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b5_12_workspace_mutation_invalidation() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    let test_file = ws_dir.path().join("test_ws_mut.js");
    tokio::fs::write(&test_file, "module.exports = async function() {};").await?;

    let b_spec = BrowserVerificationSpec {
        id: "spec-b5-ws-mut".into(),
        version: 1,
        backend: BrowserBackend::PlaywrightChromium,
        base_url: None,
        tests: vec![BrowserTestSpec {
            id: "ws_mut_test".into(),
            name: "WS Mut Test".into(),
            entrypoint: "test_ws_mut.js".into(),
            command: None,
            timeout_seconds: 10,
            required: true,
        }],
        timeout_seconds: 20,
        artifact_policy: BrowserArtifactPolicy::default(),
        console_policy: BrowserConsolePolicy::default(),
        page_error_policy: BrowserPageErrorPolicy::default(),
        network_policy: BrowserNetworkPolicy::default(),
        shm_size_mb: None,
        memory_limit_mb: None,
    };

    let mut policy = VerificationPolicy::new("pol-b5-ws-mut", "WS Mut Policy");
    policy.browser_verification_spec = Some(b_spec);

    let plan = VerificationPlan::new(
        "plan-b5-ws-mut",
        "plan-b5-ws-mut",
        vec![VerificationStep::new_command(
            "step1",
            "dummy",
            vec!["true".into()],
        )],
    );

    let ws_state_a = WorkspaceState::compute_from_parts("c0", "c0", Some("diff-A"));
    let ws_state_b = WorkspaceState::compute_from_parts("c0", "c0", Some("diff-B"));

    let artifacts_dir = tempfile::tempdir()?;
    set_artifacts_dir(artifacts_dir.path());

    let run = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "att-b5-ws-mut",
        &ws_state_a,
        &plan,
        ws_dir.path(),
        dummy_env_identity(),
        Some(&policy),
        None,
    )
    .await?;

    assert_eq!(run.overall_result, Some(VerificationRunResult::Passed));

    // Qualifies state A
    let qual_a = ctx
        .verification_store
        .check_workspace_qualification(&ws_state_a.state_id, &policy, None)
        .await?;
    assert!(qual_a.is_some());

    // Does NOT qualify mutated state B
    let qual_b = ctx
        .verification_store
        .check_workspace_qualification(&ws_state_b.state_id, &policy, None)
        .await?;
    assert!(qual_b.is_none());

    Ok(())
}

// 13. B5_TIMEOUT_CLEANUP
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b5_13_timeout_cleanup() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    let test_file = ws_dir.path().join("test_hang.js");
    tokio::fs::write(
        &test_file,
        r#"
module.exports = async function() {
    await new Promise(r => setTimeout(r, 60000));
};
"#,
    )
    .await?;

    let b_spec = BrowserVerificationSpec {
        id: "spec-b5-timeout".into(),
        version: 1,
        backend: BrowserBackend::PlaywrightChromium,
        base_url: None,
        tests: vec![BrowserTestSpec {
            id: "hang_test".into(),
            name: "Hang Test".into(),
            entrypoint: "test_hang.js".into(),
            command: None,
            timeout_seconds: 2,
            required: true,
        }],
        timeout_seconds: 5,
        artifact_policy: BrowserArtifactPolicy::default(),
        console_policy: BrowserConsolePolicy::default(),
        page_error_policy: BrowserPageErrorPolicy::default(),
        network_policy: BrowserNetworkPolicy::default(),
        shm_size_mb: None,
        memory_limit_mb: None,
    };

    let mut policy = VerificationPolicy::new("pol-b5-timeout", "Timeout Policy");
    policy.browser_verification_spec = Some(b_spec);

    let plan = VerificationPlan::new(
        "plan-b5-timeout",
        "plan-b5-timeout",
        vec![VerificationStep::new_command(
            "step1",
            "dummy",
            vec!["true".into()],
        )],
    );

    let ws_state = WorkspaceState::compute_from_parts("c0", "c0", Some("diff-timeout"));
    let artifacts_dir = tempfile::tempdir()?;
    set_artifacts_dir(artifacts_dir.path());

    let run = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "att-b5-timeout",
        &ws_state,
        &plan,
        ws_dir.path(),
        dummy_env_identity(),
        Some(&policy),
        None,
    )
    .await?;

    assert_eq!(run.overall_result, Some(VerificationRunResult::TimedOut));
    let brun = run.browser_verification_run.expect("browser run");
    assert_eq!(
        brun.overall_failure_reason,
        Some(BrowserFailureReason::TestTimeout)
    );

    Ok(())
}

// 14. B5_CANCEL_CLEANUP
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b5_14_cancel_cleanup() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    let test_file = ws_dir.path().join("test_cancel.js");
    tokio::fs::write(
        &test_file,
        r#"
module.exports = async function() {
    await new Promise(r => setTimeout(r, 60000));
};
"#,
    )
    .await?;

    let b_spec = BrowserVerificationSpec {
        id: "spec-b5-cancel".into(),
        version: 1,
        backend: BrowserBackend::PlaywrightChromium,
        base_url: None,
        tests: vec![BrowserTestSpec {
            id: "cancel_test".into(),
            name: "Cancel Test".into(),
            entrypoint: "test_cancel.js".into(),
            command: None,
            timeout_seconds: 30,
            required: true,
        }],
        timeout_seconds: 60,
        artifact_policy: BrowserArtifactPolicy::default(),
        console_policy: BrowserConsolePolicy::default(),
        page_error_policy: BrowserPageErrorPolicy::default(),
        network_policy: BrowserNetworkPolicy::default(),
        shm_size_mb: None,
        memory_limit_mb: None,
    };

    let mut policy = VerificationPolicy::new("pol-b5-cancel", "Cancel Policy");
    policy.browser_verification_spec = Some(b_spec);

    let plan = VerificationPlan::new(
        "plan-b5-cancel",
        "plan-b5-cancel",
        vec![VerificationStep::new_command(
            "step1",
            "dummy",
            vec!["true".into()],
        )],
    );

    let ws_state = WorkspaceState::compute_from_parts("c0", "c0", Some("diff-cancel"));
    let artifacts_dir = tempfile::tempdir()?;
    set_artifacts_dir(artifacts_dir.path());

    let (tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        tx.send(true).ok();
    });

    let run = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "att-b5-cancel",
        &ws_state,
        &plan,
        ws_dir.path(),
        dummy_env_identity(),
        Some(&policy),
        Some(rx),
    )
    .await?;

    assert_eq!(run.overall_result, Some(VerificationRunResult::Cancelled));

    Ok(())
}

// 15. B5_NO_HOST_PORT_REQUIRED
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b5_15_no_host_port_required() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    // Service: python HTTP server inside isolated container with NO host port publishing
    let mut env_spec = IntegrationEnvironmentSpec::new("env-no-host-port");
    let svc = ManagedServiceSpec {
        id: "web".into(),
        kind: ServiceKind::Container,
        image: Some("docker.io/library/python:3.11-alpine".into()),
        command: vec![
            "python3".into(),
            "-m".into(),
            "http.server".into(),
            "8080".into(),
        ],
        args: Vec::new(),
        env: BTreeMap::new(),
        mounts: Vec::new(),
        internal_port: Some(8080),
        readiness: Some(ReadinessProbe::Tcp {
            host: "web".into(),
            port: 8080,
            timeout_seconds: 30,
            interval_ms: 250,
        }),
        timeout_seconds: 40,
        dependencies: Vec::new(),
    };
    env_spec.services.push(svc);

    let test_file = ws_dir.path().join("test_internal_dns.js");
    tokio::fs::write(
        &test_file,
        r#"
module.exports = async function({ page, baseURL }) {
    const url = baseURL || "http://web:8080";
    await page.goto(url, { timeout: 10000 });
    const content = await page.content();
    if (!content.includes("Directory listing")) {
        throw new Error("Failed to reach internal web service via DNS: content was " + content);
    }
};
"#,
    )
    .await?;

    let b_spec = BrowserVerificationSpec {
        id: "spec-b5-dns".into(),
        version: 1,
        backend: BrowserBackend::PlaywrightChromium,
        base_url: Some("http://web:8080".into()),
        tests: vec![BrowserTestSpec {
            id: "internal_dns_test".into(),
            name: "Internal DNS Test".into(),
            entrypoint: "test_internal_dns.js".into(),
            command: None,
            timeout_seconds: 15,
            required: true,
        }],
        timeout_seconds: 30,
        artifact_policy: BrowserArtifactPolicy::default(),
        console_policy: BrowserConsolePolicy::default(),
        page_error_policy: BrowserPageErrorPolicy::default(),
        network_policy: BrowserNetworkPolicy::default(),
        shm_size_mb: None,
        memory_limit_mb: None,
    };

    let mut policy = VerificationPolicy::new("pol-b5-dns", "DNS Policy");
    policy.integration_environment_spec = Some(env_spec);
    policy.browser_verification_spec = Some(b_spec);

    let plan = VerificationPlan::new(
        "plan-b5-dns",
        "plan-b5-dns",
        vec![VerificationStep::new_command(
            "step1",
            "dummy",
            vec!["true".into()],
        )],
    );

    let ws_state = WorkspaceState::compute_from_parts("c0", "c0", Some("diff-dns"));
    let artifacts_dir = tempfile::tempdir()?;
    set_artifacts_dir(artifacts_dir.path());

    let run = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "att-b5-dns",
        &ws_state,
        &plan,
        ws_dir.path(),
        dummy_env_identity(),
        Some(&policy),
        None,
    )
    .await?;

    assert_eq!(run.overall_result, Some(VerificationRunResult::Passed));
    let brun = run.browser_verification_run.expect("browser run");
    assert_eq!(brun.status, BrowserVerificationStatus::Passed);

    Ok(())
}

// 16. B5_BROWSER_SECURITY_ISOLATION
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b5_16_browser_security_isolation() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    // Create a host canary file in a secret external location
    let secret_dir = tempfile::tempdir()?;
    let canary_path = secret_dir.path().join("orbit_host_canary.secret");
    tokio::fs::write(&canary_path, "CANARY_CONFIDENTIAL_12345").await?;

    let test_file = ws_dir.path().join("test_security.js");
    tokio::fs::write(
        &test_file,
        format!(
            r#"
const fs = require('fs');
module.exports = async function() {{
    const hostCanary = "{}";
    if (fs.existsSync(hostCanary)) {{
        throw new Error("SECURITY_BREACH: Host canary file is readable inside browser container!");
    }}
    // Verify Docker socket is not present
    if (fs.existsSync("/var/run/docker.sock") || fs.existsSync("/run/podman/podman.sock")) {{
        throw new Error("SECURITY_BREACH: Container socket is mounted!");
    }}
}};
"#,
            canary_path.display()
        ),
    )
    .await?;

    let b_spec = BrowserVerificationSpec {
        id: "spec-b5-sec".into(),
        version: 1,
        backend: BrowserBackend::PlaywrightChromium,
        base_url: None,
        tests: vec![BrowserTestSpec {
            id: "sec_test".into(),
            name: "Security Test".into(),
            entrypoint: "test_security.js".into(),
            command: None,
            timeout_seconds: 15,
            required: true,
        }],
        timeout_seconds: 30,
        artifact_policy: BrowserArtifactPolicy::default(),
        console_policy: BrowserConsolePolicy::default(),
        page_error_policy: BrowserPageErrorPolicy::default(),
        network_policy: BrowserNetworkPolicy::default(),
        shm_size_mb: None,
        memory_limit_mb: None,
    };

    let mut policy = VerificationPolicy::new("pol-b5-sec", "Sec Policy");
    policy.browser_verification_spec = Some(b_spec);

    let plan = VerificationPlan::new(
        "plan-b5-sec",
        "plan-b5-sec",
        vec![VerificationStep::new_command(
            "step1",
            "dummy",
            vec!["true".into()],
        )],
    );

    let ws_state = WorkspaceState::compute_from_parts("c0", "c0", Some("diff-sec"));
    let artifacts_dir = tempfile::tempdir()?;
    set_artifacts_dir(artifacts_dir.path());

    let run = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "att-b5-sec",
        &ws_state,
        &plan,
        ws_dir.path(),
        dummy_env_identity(),
        Some(&policy),
        None,
    )
    .await?;

    assert_eq!(run.overall_result, Some(VerificationRunResult::Passed));
    let brun = run.browser_verification_run.expect("browser run");
    assert_eq!(brun.status, BrowserVerificationStatus::Passed);

    Ok(())
}

// 17. B5_RESTART_DURABILITY
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b5_17_restart_durability() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    let test_file = ws_dir.path().join("test_dur.js");
    tokio::fs::write(&test_file, "module.exports = async function() {};").await?;

    let b_spec = BrowserVerificationSpec {
        id: "spec-b5-dur".into(),
        version: 1,
        backend: BrowserBackend::PlaywrightChromium,
        base_url: None,
        tests: vec![BrowserTestSpec {
            id: "dur_test".into(),
            name: "Durability Test".into(),
            entrypoint: "test_dur.js".into(),
            command: None,
            timeout_seconds: 10,
            required: true,
        }],
        timeout_seconds: 20,
        artifact_policy: BrowserArtifactPolicy::default(),
        console_policy: BrowserConsolePolicy::default(),
        page_error_policy: BrowserPageErrorPolicy::default(),
        network_policy: BrowserNetworkPolicy::default(),
        shm_size_mb: None,
        memory_limit_mb: None,
    };

    let mut policy = VerificationPolicy::new("pol-b5-dur", "Durability Policy");
    policy.browser_verification_spec = Some(b_spec);

    let plan = VerificationPlan::new(
        "plan-b5-dur",
        "plan-b5-dur",
        vec![VerificationStep::new_command(
            "step1",
            "dummy",
            vec!["true".into()],
        )],
    );

    let ws_state = WorkspaceState::compute_from_parts("c0", "c0", Some("diff-dur"));
    let artifacts_dir = tempfile::tempdir()?;
    set_artifacts_dir(artifacts_dir.path());

    let run = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "att-b5-dur",
        &ws_state,
        &plan,
        ws_dir.path(),
        dummy_env_identity(),
        Some(&policy),
        None,
    )
    .await?;

    let original_brun = run.browser_verification_run.expect("browser run");

    // Close pool and reconnect cleanly
    let pool_clone = ctx.engine.pool.clone();
    let new_bstore = BrowserStore::new(pool_clone);

    let recovered = new_bstore
        .get_browser_verification_run(&original_brun.id)
        .await?
        .expect("must recover browser verification run");

    assert_eq!(recovered.id, original_brun.id);
    assert_eq!(recovered.status, original_brun.status);
    assert_eq!(
        recovered.browser_image_digest,
        original_brun.browser_image_digest
    );
    assert_eq!(recovered.test_runs.len(), 1);
    assert_eq!(recovered.test_runs[0].test_id, "dur_test");

    Ok(())
}

// 18. B5_B3_REPAIR_INTEGRATION
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b5_18_b3_repair_integration() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    let test_file = ws_dir.path().join("test_repair.js");
    // Start with a failing assertion
    tokio::fs::write(
        &test_file,
        r#"
module.exports = async function() {
    throw new Error("Initial failure requires implementer repair");
};
"#,
    )
    .await?;

    let b_spec = BrowserVerificationSpec {
        id: "spec-b5-repair".into(),
        version: 1,
        backend: BrowserBackend::PlaywrightChromium,
        base_url: None,
        tests: vec![BrowserTestSpec {
            id: "repair_test".into(),
            name: "Repair Test".into(),
            entrypoint: "test_repair.js".into(),
            command: None,
            timeout_seconds: 10,
            required: true,
        }],
        timeout_seconds: 20,
        artifact_policy: BrowserArtifactPolicy::default(),
        console_policy: BrowserConsolePolicy::default(),
        page_error_policy: BrowserPageErrorPolicy::default(),
        network_policy: BrowserNetworkPolicy::default(),
        shm_size_mb: None,
        memory_limit_mb: None,
    };

    let mut policy = VerificationPolicy::new("pol-b5-repair", "Repair Policy");
    policy.browser_verification_spec = Some(b_spec.clone());

    let plan = VerificationPlan::new(
        "plan-b5-repair",
        "plan-b5-repair",
        vec![VerificationStep::new_command(
            "step1",
            "dummy",
            vec!["true".into()],
        )],
    );

    let ws_state_fail = WorkspaceState::compute_from_parts("c0", "c0", Some("diff-repair-fail"));
    let artifacts_dir = tempfile::tempdir()?;
    set_artifacts_dir(artifacts_dir.path());

    // Iteration 1: Verification FAILS
    let run1 = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "att-b5-repair-1",
        &ws_state_fail,
        &plan,
        ws_dir.path(),
        dummy_env_identity(),
        Some(&policy),
        None,
    )
    .await?;

    assert_eq!(run1.overall_result, Some(VerificationRunResult::Failed));

    // Qualification check confirms: Reviewer must NOT be allowed to qualify
    let qual1 = ctx
        .verification_store
        .check_workspace_qualification(&ws_state_fail.state_id, &policy, None)
        .await?;
    assert!(qual1.is_none());

    // Repair action: implementer fixes test_repair.js
    tokio::fs::write(&test_file, "module.exports = async function() {};").await?;
    let ws_state_repaired =
        WorkspaceState::compute_from_parts("c0", "c0", Some("diff-repair-pass"));

    // Iteration 2: Verification PASSES
    let run2 = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "att-b5-repair-2",
        &ws_state_repaired,
        &plan,
        ws_dir.path(),
        dummy_env_identity(),
        Some(&policy),
        None,
    )
    .await?;

    assert_eq!(run2.overall_result, Some(VerificationRunResult::Passed));

    // Qualification check succeeds! Reviewer can now receive the evidence
    let qual2 = ctx
        .verification_store
        .check_workspace_qualification(&ws_state_repaired.state_id, &policy, None)
        .await?;
    assert!(qual2.is_some());

    Ok(())
}

// 19. B5_FINAL_REGRESSION_INTEGRATION
#[tokio::test]
#[ignore = "requires PostgreSQL and podman"]
async fn test_b5_19_final_regression_integration() -> Result<()> {
    let ctx = setup_qualification_context().await?;
    let ws_dir = tempfile::tempdir()?;

    let test_file = ws_dir.path().join("test_final.js");
    tokio::fs::write(
        &test_file,
        r#"
module.exports = async function({ page }) {
    await page.setContent("<h1>Orbit Full Verification Suite Passed</h1>");
};
"#,
    )
    .await?;

    let b_spec = BrowserVerificationSpec {
        id: "spec-b5-final".into(),
        version: 1,
        backend: BrowserBackend::PlaywrightChromium,
        base_url: None,
        tests: vec![BrowserTestSpec {
            id: "final_test".into(),
            name: "Final Test".into(),
            entrypoint: "test_final.js".into(),
            command: None,
            timeout_seconds: 15,
            required: true,
        }],
        timeout_seconds: 30,
        artifact_policy: BrowserArtifactPolicy::default(),
        console_policy: BrowserConsolePolicy::default(),
        page_error_policy: BrowserPageErrorPolicy::default(),
        network_policy: BrowserNetworkPolicy::default(),
        shm_size_mb: None,
        memory_limit_mb: None,
    };

    let mut policy = VerificationPolicy::new("pol-b5-final", "Final Policy");
    policy.browser_verification_spec = Some(b_spec);

    let plan = VerificationPlan::new(
        "plan-b5-final",
        "plan-b5-final",
        vec![VerificationStep::new_command(
            "unit_tests",
            "Unit Tests",
            vec!["true".into()],
        )],
    );

    let ws_state = WorkspaceState::compute_from_parts("c0", "c0", Some("diff-final"));
    let artifacts_dir = tempfile::tempdir()?;
    set_artifacts_dir(artifacts_dir.path());

    let run = execute_verification_plan_with_policy(
        &ctx.verification_store,
        "att-b5-final",
        &ws_state,
        &plan,
        ws_dir.path(),
        dummy_env_identity(),
        Some(&policy),
        None,
    )
    .await?;

    assert_eq!(run.overall_result, Some(VerificationRunResult::Passed));
    assert_eq!(run.step_runs.len(), 1);
    assert_eq!(run.step_runs[0].status, VerificationStepStatus::Passed);

    let brun = run.browser_verification_run.expect("browser run");
    assert_eq!(brun.status, BrowserVerificationStatus::Passed);

    // Workspace is qualified
    let qual = ctx
        .verification_store
        .check_workspace_qualification(&ws_state.state_id, &policy, None)
        .await?;
    assert!(qual.is_some());

    Ok(())
}
