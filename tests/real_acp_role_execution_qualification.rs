//! Phase B3.2 Qualification Test Suite: Real ACP Role Execution.
//! Validates:
//! 1. Durable agent execution persistence in `orbit_agent_executions`.
//! 2. Role permission boundaries: Read-only roles denied write operations.
//! 3. Read-write role allows mutation.
//! 4. Structured handoff validation: strictly parses envelopes and rejects invalid formats.
//! 5. Fail-safe verification policy: workflows without authoritative policies fail safely.
//! 6. Simulated execution is explicitly injected and cannot be selected by environment.

use anyhow::{Context, Result, ensure};
use orbit::{acp_wire::Wire, model::id, workflow::*, workflow_coordinator::*};
use sqlx::PgPool;
use std::{
    collections::BTreeMap,
    ops::Deref,
    path::Path,
    sync::{Arc, Mutex},
};

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
    let database = common::DisposablePgTestContext::create("b32", 3).await?;
    let store = WorkflowStore::new(database.engine.pool.clone());
    Ok(TestContext { database, store })
}

async fn teardown_test(ctx: TestContext) -> Result<()> {
    ctx.database.teardown().await
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

#[derive(Debug)]
struct OfflineRoleCallbacks {
    role_id: String,
    read_response: serde_json::Value,
    write_response: serde_json::Value,
    tool_calls: u64,
    tool_successes: u64,
    tool_failures: u64,
}

#[derive(Default)]
struct DeterministicAcpTransport {
    callbacks: Mutex<Vec<OfflineRoleCallbacks>>,
}

impl DeterministicAcpTransport {
    async fn run_role(
        &self,
        pool: &PgPool,
        wf_run: &WorkflowRun,
        role_exec: &RoleExecution,
        role: &RoleDefinition,
        repo_path: &Path,
    ) -> Result<String> {
        let (orbit_read, peer_write) = tokio::io::duplex(65_536);
        let (peer_read, orbit_write) = tokio::io::duplex(65_536);
        let mut orbit_wire = Wire::new(orbit_read, orbit_write, 65_536);
        let mut peer_wire = Wire::new(peer_read, peer_write, 65_536);

        let mut state = AcpTurnState::new(repo_path, role.workspace_access);
        state.role_id = Some(role.role_id.clone());
        state.workspace_identity = wf_run.repository_path.clone();
        state.pool = Some(pool);
        state.wf_attempt_id = Some(wf_run.attempt_id.clone());
        state.role_exec_id = Some(role_exec.id.clone());

        let read_response = Self::dispatch_callback(
            &mut orbit_wire,
            &mut peer_wire,
            &mut state,
            "fixture-read",
            "fs/read_text_file",
            serde_json::json!({"path":"README.md"}),
        )
        .await?;
        let read_content = read_response["result"]["content"]
            .as_str()
            .context("offline ACP read callback did not return file content")?;
        ensure!(
            read_content.contains("offline fixture baseline"),
            "offline ACP callback did not read the fixture repository"
        );

        let write_response = Self::dispatch_callback(
            &mut orbit_wire,
            &mut peer_wire,
            &mut state,
            "fixture-write",
            "fs/write_text_file",
            serde_json::json!({"path":"README.md","content":"offline fixture baseline\n"}),
        )
        .await?;

        let payload = match role.role_id.as_str() {
            "planner" => serde_json::to_value(PlanHandoff {
                summary: "Inspected the fixture repository".into(),
                affected_areas: vec!["README.md".into()],
                implementation_steps: vec!["Keep the fixture unchanged".into()],
                expected_files: vec![],
                risks: vec![],
                verification_notes: vec!["Verification remains profile-gated".into()],
                open_questions: vec![],
            })?,
            "implementer" => serde_json::to_value(ImplementationHandoff {
                summary: "Completed the no-change offline fixture turn".into(),
                changed_files: vec![],
                tests_added_or_modified: vec![],
                exploratory_commands: vec![],
                known_limitations: vec![],
                verification_notes: vec!["Verification remains profile-gated".into()],
            })?,
            other => anyhow::bail!("unexpected role in offline ACP fixture: {other}"),
        };
        let handoff = format!(
            "{ORBIT_HANDOFF_START}\n{}\n{ORBIT_HANDOFF_END}",
            serde_json::to_string(&payload)?
        );
        peer_wire
            .notify(
                "session/update",
                serde_json::json!({"update":{"text":handoff}}),
            )
            .await?;
        let update = orbit_wire.read().await?;
        handle_acp_message(&mut orbit_wire, &mut state, update).await?;

        self.callbacks
            .lock()
            .expect("offline ACP callback audit mutex poisoned")
            .push(OfflineRoleCallbacks {
                role_id: role.role_id.clone(),
                read_response,
                write_response,
                tool_calls: state.tool_calls,
                tool_successes: state.tool_successes,
                tool_failures: state.tool_failures,
            });

        Ok(state.agent_output)
    }

    async fn dispatch_callback(
        orbit_wire: &mut Wire,
        peer_wire: &mut Wire,
        state: &mut AcpTurnState<'_>,
        request_id: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        peer_wire
            .send(serde_json::json!({
                "jsonrpc":"2.0",
                "id":request_id,
                "method":method,
                "params":params
            }))
            .await?;
        let callback = orbit_wire.read().await?;
        handle_acp_message(orbit_wire, state, callback).await?;
        peer_wire.read().await
    }
}

struct OfflineAcpRoleExecutor {
    transport: Arc<DeterministicAcpTransport>,
}

impl OfflineAcpRoleExecutor {
    fn new(transport: Arc<DeterministicAcpTransport>) -> Self {
        Self { transport }
    }
}

#[async_trait::async_trait]
impl RoleAgentExecutor for OfflineAcpRoleExecutor {
    async fn execute_role(
        &self,
        pool: &PgPool,
        wf_run: &WorkflowRun,
        role_exec: &RoleExecution,
        role: &RoleDefinition,
        target: &ResolvedExecutionTarget,
        _task_text: &str,
        repo_path: &Path,
        input_handoff: Option<&HandoffArtifact>,
        _cancellation: tokio::sync::watch::Receiver<bool>,
    ) -> Result<RoleExecutionOutcome> {
        ensure!(
            target.runtime_interface.ends_with("-acp"),
            "offline fixture expected an ACP runtime target"
        );
        if role.role_id == "planner" {
            ensure!(
                input_handoff.is_none(),
                "planner received an unexpected handoff"
            );
        } else {
            let handoff = input_handoff.context("implementer did not receive its durable plan")?;
            ensure!(
                handoff.handoff_type == HandoffType::Plan,
                "implementer received a non-plan handoff"
            );
        }

        let raw_output = self
            .transport
            .run_role(pool, wf_run, role_exec, role, repo_path)
            .await?;
        Ok(RoleExecutionOutcome {
            raw_output,
            agent_execution_ids: vec![],
            termination_reason: Some("deterministic offline ACP peer".into()),
        })
    }
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn b32_01_agent_executions_schema_persists_durable_metrics() -> Result<()> {
    let ctx = setup_test().await?;

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
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn b32_04_host_verification_is_blocked_without_pinned_profile() -> Result<()> {
    let ctx = setup_test().await?;
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

    // Verification cannot reach the host executor without a pinned container profile.
    let res = coord.step(&wf.id).await;
    assert!(
        res.is_err(),
        "expected coordinator to reject unpinned verification"
    );
    let err_msg = res.unwrap_err().to_string();
    assert!(
        err_msg.contains("VERIFICATION_PROFILE_REQUIRED"),
        "expected VERIFICATION_PROFILE_REQUIRED, got: {err_msg}"
    );
    assert_eq!(
        ctx.store.get_workflow_run(&wf.id).await?.unwrap().status,
        WorkflowStage::Verifying
    );

    teardown_test(ctx).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn b32_05_simulation_is_explicitly_injected() -> Result<()> {
    let ctx = setup_test().await?;

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

    let executor = SimulatedRoleExecutor::with_approval();
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
            tokio::sync::watch::channel(false).1,
        )
        .await?;

    assert!(outcome.raw_output.contains(ORBIT_HANDOFF_START));
    assert_eq!(outcome.termination_reason, Some("completed".into()));

    teardown_test(ctx).await?;
    Ok(())
}

#[tokio::test]
async fn b32_06_orbit_mock_acp_cannot_switch_the_real_executor() -> Result<()> {
    use std::time::Duration;

    let previous = std::env::var_os("ORBIT_MOCK_ACP");
    unsafe { std::env::set_var("ORBIT_MOCK_ACP", "1") };

    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(250))
        .connect_lazy_with(
            sqlx::postgres::PgConnectOptions::new()
                .host("127.0.0.1")
                .port(1)
                .username("orbit")
                .database("orbit"),
        );
    let role = RoleDefinition::planner_v1();
    let target = ResolvedExecutionTarget {
        provider: "codex".into(),
        runtime_interface: "codex-acp".into(),
        credential_id: Some("codex-main".into()),
        credential_generation: Some(1),
        requested_model: Some("gpt-6-luna".into()),
        resolved_model: Some("gpt-6-luna".into()),
        runtime_image_digest: None,
        resolution_reason: "environment-switch regression test".into(),
    };
    let wf = WorkflowRun {
        id: "wf-env-switch".into(),
        task_id: "task-env-switch".into(),
        attempt_id: "attempt-env-switch".into(),
        workflow_kind: "software_change".into(),
        workflow_version: 1,
        status: WorkflowStage::Planning,
        current_stage: "PLANNING".into(),
        iteration: 0,
        max_iterations: 1,
        current_workspace_state_id: None,
        verification_policy_id: None,
        verification_policy_version: None,
        verification_policy_digest: None,
        regression_policy_id: None,
        regression_policy_version: None,
        regression_policy_digest: None,
        selection_policy_id: None,
        selection_policy_version: None,
        selection_policy_digest: None,
        task_prompt: Some("must not be simulated".into()),
        repository_path: Some(".".into()),
        base_revision: Some("HEAD".into()),
        failure_reason: None,
        cancellation_reason: None,
        started_at_ms: 0,
        finished_at_ms: None,
    };
    let role_exec = RoleExecution {
        id: "role-env-switch".into(),
        workflow_run_id: wf.id.clone(),
        role_id: role.role_id.clone(),
        role_version: role.version,
        role_digest: role.digest(),
        stage: "PLANNING".into(),
        iteration: 0,
        status: RoleExecutionStatus::Running,
        input_workspace_state_id: None,
        output_workspace_state_id: None,
        resolved_target: Some(target.clone()),
        agent_execution_ids: vec![],
        handoff_input_id: None,
        handoff_output_id: None,
        started_at_ms: 0,
        finished_at_ms: None,
        termination_reason: None,
        failure_message: None,
    };

    let outcome = RealAcpRoleExecutor
        .execute_role(
            &pool,
            &wf,
            &role_exec,
            &role,
            &target,
            "must not be simulated",
            Path::new("."),
            None,
            tokio::sync::watch::channel(false).1,
        )
        .await;

    unsafe {
        if let Some(value) = previous {
            std::env::set_var("ORBIT_MOCK_ACP", value);
        } else {
            std::env::remove_var("ORBIT_MOCK_ACP");
        }
    }
    assert!(
        outcome.is_err(),
        "real executor reached real credential lookup"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn b32_07_full_coordinator_offline_acp_callbacks_handoffs_and_verification_gate() -> Result<()>
{
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;

    let repo = common::TemporaryGitRepo::create()?;
    let repo_path = repo.path().to_string_lossy().into_owned();
    let attempt_id = format!("att-{}", id());
    let wf = ctx
        .store
        .create_workflow_run_full(
            "b32-offline-acp",
            &attempt_id,
            2,
            None,
            None,
            None,
            Some("Inspect the fixture and return a no-change handoff"),
            Some(&repo_path),
            Some(repo.baseline_revision()),
        )
        .await?;

    let transport = Arc::new(DeterministicAcpTransport::default());
    let executor = Arc::new(OfflineAcpRoleExecutor::new(transport.clone()));
    let coordinator = WorkflowCoordinator::new(ctx.engine.pool.clone(), executor);

    assert_eq!(
        coordinator.step(&wf.id).await?,
        WorkflowStepResult::Advanced {
            from: WorkflowStage::Created,
            to: WorkflowStage::Planning,
        }
    );
    assert_eq!(
        coordinator.step(&wf.id).await?,
        WorkflowStepResult::Advanced {
            from: WorkflowStage::Planning,
            to: WorkflowStage::Implementing,
        }
    );
    assert_eq!(
        coordinator.step(&wf.id).await?,
        WorkflowStepResult::Advanced {
            from: WorkflowStage::Implementing,
            to: WorkflowStage::Verifying,
        }
    );

    let run = ctx
        .store
        .get_workflow_run(&wf.id)
        .await?
        .context("workflow disappeared")?;
    assert_eq!(run.status, WorkflowStage::Verifying);
    let workspace_state_id = run
        .current_workspace_state_id
        .as_deref()
        .context("implementer did not persist a workspace state")?;

    let plan_artifact = ctx
        .store
        .get_latest_handoff_of_type(&wf.id, HandoffType::Plan)
        .await?
        .context("coordinator did not persist the planner handoff")?;
    let plan: PlanHandoff = serde_json::from_value(plan_artifact.structured_payload)?;
    assert_eq!(plan.summary, "Inspected the fixture repository");

    let implementation_artifact = ctx
        .store
        .get_latest_handoff_of_type(&wf.id, HandoffType::Implementation)
        .await?
        .context("coordinator did not persist the implementation handoff")?;
    assert_eq!(
        implementation_artifact.workspace_state_id.as_deref(),
        Some(workspace_state_id)
    );
    let implementation: ImplementationHandoff =
        serde_json::from_value(implementation_artifact.structured_payload)?;
    assert_eq!(
        implementation.summary,
        "Completed the no-change offline fixture turn"
    );

    {
        let callback_evidence = transport
            .callbacks
            .lock()
            .expect("offline ACP callback audit mutex poisoned");
        assert_eq!(
            callback_evidence.len(),
            2,
            "both coordinator role turns ran"
        );
        assert_eq!(callback_evidence[0].role_id, "planner");
        assert_eq!(callback_evidence[1].role_id, "implementer");
        for evidence in callback_evidence.iter() {
            assert!(
                evidence.read_response["result"]["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("offline fixture baseline")),
                "the ACP read callback must reach repository tools"
            );
            assert_eq!(evidence.tool_calls, 2);
            if evidence.role_id == "planner" {
                assert_eq!(evidence.tool_successes, 1);
                assert_eq!(evidence.tool_failures, 1);
            } else {
                assert_eq!(evidence.tool_successes, 2);
                assert_eq!(evidence.tool_failures, 0);
            }
        }
        assert!(
            callback_evidence[0].write_response["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains(orbit::tool_surface::ERR_READ_ONLY_ROLE)),
            "planner mutation must be denied by role authorization"
        );
        assert!(
            callback_evidence[1].write_response.get("result").is_some(),
            "implementer with the persisted role execution lock must be allowed"
        );
    }
    assert_eq!(
        std::fs::read_to_string(repo.path().join("README.md"))?,
        "offline fixture baseline\n",
        "denied ACP writes must leave the repository unchanged"
    );

    let verify = coordinator.step(&wf.id).await.unwrap_err();
    assert!(
        format!("{verify:#}").contains("VERIFICATION_PROFILE_REQUIRED"),
        "verification must fail closed without a pinned profile: {verify:#}"
    );
    assert_eq!(
        ctx.store
            .get_workflow_run(&wf.id)
            .await?
            .context("workflow disappeared")?
            .status,
        WorkflowStage::Verifying,
        "a denied verification transition must retain its current stage"
    );

    teardown_test(ctx).await?;
    Ok(())
}
