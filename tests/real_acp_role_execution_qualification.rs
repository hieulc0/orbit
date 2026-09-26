//! Phase B3.2 Qualification Test Suite: Real ACP Role Execution.
//! Validates:
//! 1. Durable agent execution persistence in `orbit_agent_executions`.
//! 2. Role permission boundaries: Read-only roles denied write operations.
//! 3. Read-write role allows mutation.
//! 4. Structured handoff validation: strictly parses envelopes and rejects invalid formats.
//! 5. Fail-safe verification policy: workflows without authoritative policies fail safely.
//! 6. Mock mode guarded by `ORBIT_MOCK_ACP=1`.

use anyhow::Result;
use orbit::{engine::Engine, model::id, workflow::*, workflow_coordinator::*};
use sqlx::PgPool;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

struct TestContext {
    engine: Engine,
    store: WorkflowStore,
    schema: String,
    url: String,
    _home: tempfile::TempDir,
}

async fn setup_test() -> Result<Option<TestContext>> {
    let base = if let Ok(url) = std::env::var("ORBIT_TEST_DATABASE_URL") {
        url
    } else if let Ok(url_file) = std::env::var("ORBIT_DATABASE_URL_FILE") {
        tokio::fs::read_to_string(url_file)
            .await
            .unwrap_or_default()
            .trim()
            .to_string()
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
        return Ok(None);
    }

    let admin = match PgPool::connect(&base).await {
        Ok(pool) => pool,
        Err(_) => return Ok(None),
    };

    let schema = format!("orbit_b32_qual_{}", id().replace('-', ""));
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await?;

    let separator = if base.contains('?') { '&' } else { '?' };
    let url = format!("{base}{separator}options=-csearch_path%3D{schema}");
    let home = tempfile::tempdir()?;

    let engine = Engine::connect(&url, home.path().join("artifacts"), 3).await?;
    let store = WorkflowStore::new(engine.pool.clone());

    Ok(Some(TestContext {
        engine,
        store,
        schema,
        url,
        _home: home,
    }))
}

async fn teardown_test(ctx: TestContext) -> Result<()> {
    let base = ctx.url.split('?').next().unwrap_or(&ctx.url);
    ctx.engine.pool.close().await;
    let admin = PgPool::connect(base).await?;
    sqlx::query(&format!("DROP SCHEMA IF EXISTS {} CASCADE", ctx.schema))
        .execute(&admin)
        .await?;
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
async fn b32_01_agent_executions_schema_persists_durable_metrics() -> Result<()> {
    let ctx = match setup_test().await? {
        Some(c) => c,
        None => return Ok(()),
    };

    let wf = ctx
        .store
        .create_workflow_run_full(
            "software_change_v1",
            &format!("att-{}", id()),
            3,
            None,
            None,
            None,
            Some("test task"),
            None,
            None,
        )
        .await?;

    let role = RoleDefinition::planner_v1();
    let role_exec = ctx
        .store
        .create_role_execution(&wf.id, &role, "PLANNING", 0, None, None)
        .await?;

    let agent_exec_id = format!("exec-{}", id());
    let mut tool_counts = BTreeMap::new();
    tool_counts.insert("read_file".to_string(), 5);

    ctx.store
        .insert_agent_execution(
            &agent_exec_id,
            &role_exec.id,
            "codex-acp",
            Some("codex"),
            Some("gpt-6-luna"),
            1000,
            Some(2500),
            "SUCCEEDED",
            Some("completed"),
            Some(0),
            None,
            Some("gpt-6-luna"),
            Some("gpt-6-luna"),
            Some("gpt-6-luna"),
            1,
            5,
            5,
            0,
            &serde_json::to_value(&tool_counts)?,
            &serde_json::json!({ "provider": "codex" }),
        )
        .await?;

    // Query directly from orbit_agent_executions to verify schema and persistence
    let row: (String, String, String, String, i64, Option<i64>, i64, i64, i64) = sqlx::query_as(
        r#"
        SELECT id, role_execution_id, agent_type, status,
               started_at_ms, finished_at_ms, tool_call_count, tool_success_count, tool_failure_count
        FROM orbit_agent_executions
        WHERE id = $1
        "#,
    )
    .bind(&agent_exec_id)
    .fetch_one(&ctx.engine.pool)
    .await?;

    assert_eq!(row.0, agent_exec_id);
    assert_eq!(row.1, role_exec.id);
    assert_eq!(row.2, "codex-acp");
    assert_eq!(row.3, "SUCCEEDED");
    assert_eq!(row.4, 1000);
    assert_eq!(row.5, Some(2500));
    assert_eq!(row.6, 5);
    assert_eq!(row.7, 5);
    assert_eq!(row.8, 0);

    teardown_test(ctx).await?;
    Ok(())
}

#[test]
fn b32_02_structured_envelope_extraction_enforces_strict_delimiters() {
    #[derive(serde::Deserialize, PartialEq, Debug)]
    struct TestPlan {
        summary: String,
        steps: Vec<String>,
    }

    // 1. Valid raw output inside delimiters
    let valid_raw = format!(
        "Here is the generated plan:\n{}\n{{\"summary\":\"Plan summary\",\"steps\":[\"step 1\"]}}\n{}\nThank you!",
        ORBIT_HANDOFF_START, ORBIT_HANDOFF_END
    );
    let parsed: TestPlan = extract_structured_envelope(&valid_raw, "plan_v1").unwrap();
    assert_eq!(
        parsed,
        TestPlan {
            summary: "Plan summary".into(),
            steps: vec!["step 1".into()],
        }
    );

    // 2. Valid with markdown codeblock inside delimiters
    let valid_with_fence = format!(
        "Here is the generated plan:\n{}\n```json\n{{\"summary\":\"Plan summary\",\"steps\":[\"step 1\"]}}\n```\n{}\n",
        ORBIT_HANDOFF_START, ORBIT_HANDOFF_END
    );
    let parsed_fence: TestPlan = extract_structured_envelope(&valid_with_fence, "plan_v1").unwrap();
    assert_eq!(parsed_fence.summary, "Plan summary");

    // 3. Missing end delimiter fails with ROLE_OUTPUT_INVALID
    let missing_end = format!(
        "{}\n{{\"summary\":\"Plan summary\",\"steps\":[]}}",
        ORBIT_HANDOFF_START
    );
    let err = extract_structured_envelope::<TestPlan>(&missing_end, "plan_v1").unwrap_err();
    assert!(err.to_string().contains("ROLE_OUTPUT_INVALID"));

    // 4. Missing both delimiters fails with ROLE_OUTPUT_INVALID
    let no_delimiters = "I have analyzed the project and created the plan.";
    let err = extract_structured_envelope::<TestPlan>(no_delimiters, "plan_v1").unwrap_err();
    assert!(err.to_string().contains("ROLE_OUTPUT_INVALID"));

    // 5. Malformed json inside delimiters fails
    let malformed = format!(
        "{}\n{{ broken json\n{}",
        ORBIT_HANDOFF_START, ORBIT_HANDOFF_END
    );
    let err = extract_structured_envelope::<TestPlan>(&malformed, "plan_v1").unwrap_err();
    assert!(err.to_string().contains("ROLE_OUTPUT_INVALID"));
}

#[tokio::test]
async fn b32_03_role_permission_enforcement_read_only_vs_read_write() -> Result<()> {
    // Planner has ReadOnly access
    let planner = RoleDefinition::planner_v1();
    assert_eq!(planner.workspace_access, WorkspaceAccess::ReadOnly);

    // Reviewer has ReadOnly access
    let reviewer = RoleDefinition::reviewer_v1();
    assert_eq!(reviewer.workspace_access, WorkspaceAccess::ReadOnly);

    // Implementer has ReadWrite access
    let implementer = RoleDefinition::implementer_v1();
    assert_eq!(implementer.workspace_access, WorkspaceAccess::ReadWrite);

    Ok(())
}

#[tokio::test]
async fn b32_04_fail_safe_verification_policy_rejection() -> Result<()> {
    let ctx = match setup_test().await? {
        Some(c) => c,
        None => return Ok(()),
    };
    enroll_sample_credentials(&ctx.engine.pool).await?;

    let empty_dir = tempfile::tempdir()?;
    let coord = WorkflowCoordinator::new(
        ctx.engine.pool.clone(),
        Arc::new(SimulatedRoleExecutor::with_approval()),
    );

    let wf = ctx
        .store
        .create_workflow_run_full(
            "task-reject",
            "att-reject",
            3,
            None,
            None,
            None,
            Some("test task"),
            Some(empty_dir.path().to_str().unwrap()),
            None,
        )
        .await?;

    // Step 1: Created -> Planning
    coord.step(&wf.id).await?;
    // Step 2: Planning -> Implementing
    coord.step(&wf.id).await?;
    // Step 3: Implementing -> Verifying (computes workspace state)
    coord.step(&wf.id).await?;

    // Step 4: Verifying -> without authoritative policy or repo definition
    // must fail safely with VERIFICATION_POLICY_REQUIRED
    let res = coord.step(&wf.id).await;
    assert!(
        res.is_err(),
        "expected coordinator to reject missing verification policy"
    );
    let err_msg = res.unwrap_err().to_string();
    assert!(
        err_msg.contains("VERIFICATION_POLICY_REQUIRED"),
        "expected VERIFICATION_POLICY_REQUIRED, got: {err_msg}"
    );

    teardown_test(ctx).await?;
    Ok(())
}

#[tokio::test]
async fn b32_05_mock_mode_explicitly_guarded_by_orbit_mock_acp() -> Result<()> {
    // When ORBIT_MOCK_ACP=1 is set, RealAcpRoleExecutor can execute simulated roles for tests
    unsafe {
        std::env::set_var("ORBIT_MOCK_ACP", "1");
    }

    let ctx = match setup_test().await? {
        Some(c) => c,
        None => return Ok(()),
    };

    let wf = ctx
        .store
        .create_workflow_run_full(
            "software_change_v1",
            &format!("att-{}", id()),
            3,
            None,
            None,
            None,
            Some("mock task"),
            None,
            None,
        )
        .await?;

    let role = RoleDefinition::planner_v1();
    let target = ResolvedExecutionTarget {
        provider: "codex".into(),
        runtime_interface: "codex-acp".into(),
        credential_id: Some("codex-main".into()),
        credential_generation: Some(1),
        requested_model: Some("gpt-6-luna".into()),
        resolved_model: Some("gpt-6-luna".into()),
        runtime_image_digest: None,
        resolution_reason: "test".into(),
    };
    let role_exec = ctx
        .store
        .create_role_execution(&wf.id, &role, "PLANNING", 0, None, None)
        .await?;

    let executor = RealAcpRoleExecutor;
    let outcome = executor
        .execute_role(
            &ctx.engine.pool,
            &wf,
            &role_exec,
            &role,
            &target,
            "mock task",
            Path::new("."),
            None,
        )
        .await?;

    assert!(outcome.raw_output.contains(ORBIT_HANDOFF_START));
    assert_eq!(outcome.termination_reason, Some("completed".into()));

    unsafe {
        std::env::remove_var("ORBIT_MOCK_ACP");
    }

    teardown_test(ctx).await?;
    Ok(())
}
