//! Phase B3.2 Qualification Test Suite: Real ACP Role Execution.
//! Validates:
//! 1. Durable agent execution persistence in `orbit_agent_executions`.
//! 2. Role permission boundaries: Read-only roles denied write operations.
//! 3. Read-write role allows mutation.
//! 4. Structured handoff validation: strictly parses envelopes and rejects invalid formats.
//! 5. Fail-safe verification policy: workflows without authoritative policies fail safely.
//! 6. Simulated execution is explicitly injected and cannot be selected by environment.

use anyhow::{Context, Result, ensure};
use orbit::{
    acp_wire::Wire,
    model::id,
    regression_strategy::{
        RegressionFallbackBehavior, RegressionPolicy, RegressionStore, SelectionPolicy,
        VerificationCheck, VerificationTier,
    },
    verification::{
        AllowedCommand, EnvironmentIdentity, VerificationCachePolicy, VerificationNetworkPolicy,
        VerificationPolicy, VerificationRunResult, VerificationStepStatus,
    },
    workflow::*,
    workflow_coordinator::*,
};
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
    // These catalog-only representations let the production resolver run in
    // coordinator tests; the explicitly injected offline executor never reads
    // their synthetic locators or contacts a provider.
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
    let acp_representation_id1 = id();
    let acp_representation_locator1 =
        format!("credential://{cred_id1}/generation/1/{acp_representation_id1}");
    sqlx::query("INSERT INTO orbit_credential_representations(id, credential_id, generation, interface, auth_type, state, secret_locator, last_validated_at) VALUES($1, $2, 1, 'acp', 'local-session', 'stored', $3, TIMESTAMPTZ '2000-01-01 00:00:00+00')")
        .bind(&acp_representation_id1)
        .bind(&cred_id1)
        .bind(&acp_representation_locator1)
        .execute(&mut *tx)
        .await?;

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
    let acp_representation_id2 = id();
    let acp_representation_locator2 =
        format!("credential://{cred_id2}/generation/1/{acp_representation_id2}");
    sqlx::query("INSERT INTO orbit_credential_representations(id, credential_id, generation, interface, auth_type, state, secret_locator, last_validated_at) VALUES($1, $2, 1, 'acp', 'oauth', 'stored', $3, TIMESTAMPTZ '2000-01-01 00:00:00+00')")
        .bind(&acp_representation_id2)
        .bind(&cred_id2)
        .bind(&acp_representation_locator2)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

#[derive(Debug)]
struct OfflineRoleCallbacks {
    role_id: String,
    target: ResolvedExecutionTarget,
    workspace_state_id: Option<String>,
    input_handoff_type: Option<HandoffType>,
    input_handoff_workspace_state_id: Option<String>,
    read_response: serde_json::Value,
    write_responses: Vec<serde_json::Value>,
    diff_response: Option<serde_json::Value>,
    provider_reply_type: String,
    cleanup_confirmed: bool,
    tool_calls: u64,
    tool_successes: u64,
    tool_failures: u64,
}

#[derive(Default)]
struct DeterministicAcpTransport {
    callbacks: Mutex<Vec<OfflineRoleCallbacks>>,
}

struct OfflineRoleContext<'a> {
    pool: &'a PgPool,
    wf_run: &'a WorkflowRun,
    role_exec: &'a RoleExecution,
    role: &'a RoleDefinition,
    target: &'a ResolvedExecutionTarget,
    repo_path: &'a Path,
    input_handoff: Option<&'a HandoffArtifact>,
}

impl DeterministicAcpTransport {
    async fn run_role(&self, context: OfflineRoleContext<'_>) -> Result<String> {
        let OfflineRoleContext {
            pool,
            wf_run,
            role_exec,
            role,
            target,
            repo_path,
            input_handoff,
        } = context;
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
        let expected_read = if role.role_id == "reviewer" {
            "offline fixture candidate: full-coordinator"
        } else {
            "offline fixture baseline"
        };
        ensure!(
            read_content.contains(expected_read),
            "{} received an unexpected repository candidate",
            role.role_id
        );

        let diff_response = if role.role_id == "reviewer" {
            let base = wf_run
                .base_revision
                .as_deref()
                .context("reviewer fixture has no pinned base revision")?;
            let response = Self::dispatch_callback(
                &mut orbit_wire,
                &mut peer_wire,
                &mut state,
                "fixture-candidate-diff",
                "git/diff",
                serde_json::json!({"base":base}),
            )
            .await?;
            let diff = response["result"]["diff"]
                .as_str()
                .context("reviewer ACP diff callback did not return diff content")?;
            ensure!(
                diff.contains("offline fixture candidate: full-coordinator")
                    && diff.contains("offline full check passed"),
                "reviewer did not receive the intended candidate diff"
            );
            Some(response)
        } else {
            None
        };

        let mut write_responses = Vec::new();
        match role.role_id.as_str() {
            "planner" | "reviewer" => {
                let response = Self::dispatch_callback(
                    &mut orbit_wire,
                    &mut peer_wire,
                    &mut state,
                    "fixture-denied-write",
                    "fs/write_text_file",
                    serde_json::json!({"path":"README.md","content":"unauthorized fixture mutation\n"}),
                )
                .await?;
                write_responses.push(response);
            }
            "implementer" => {
                for (request_id, path, content) in [
                    (
                        "fixture-write-readme",
                        "README.md",
                        "offline fixture candidate: full-coordinator\n",
                    ),
                    (
                        "fixture-write-check",
                        "test.sh",
                        "#!/bin/sh\nset -eu\nexpected='offline fixture candidate: full-coordinator'\nactual=$(cat README.md)\ntest \"$actual\" = \"$expected\"\nprintf 'offline full check passed: %s\\n' \"$actual\"\n",
                    ),
                ] {
                    write_responses.push(
                        Self::dispatch_callback(
                            &mut orbit_wire,
                            &mut peer_wire,
                            &mut state,
                            request_id,
                            "fs/write_text_file",
                            serde_json::json!({"path":path,"content":content}),
                        )
                        .await?,
                    );
                }
            }
            other => anyhow::bail!("unexpected role in offline ACP fixture: {other}"),
        }

        let (provider_reply_type, payload) = match role.role_id.as_str() {
            "planner" => serde_json::to_value(PlanHandoff {
                summary: "Inspected the offline candidate contract".into(),
                affected_areas: vec!["README.md".into(), "test.sh".into()],
                implementation_steps: vec![
                    "Update the README candidate marker".into(),
                    "Update test.sh to assert the candidate marker".into(),
                ],
                expected_files: vec!["README.md".into(), "test.sh".into()],
                risks: vec![],
                verification_notes: vec!["Run the pinned candidate check".into()],
                open_questions: vec![],
            })
            .map(|payload| ("PlanHandoff", payload))?,
            "implementer" => serde_json::to_value(ImplementationHandoff {
                summary: "Updated the candidate and its deterministic check".into(),
                changed_files: vec!["README.md".into(), "test.sh".into()],
                tests_added_or_modified: vec!["test.sh".into()],
                exploratory_commands: vec![],
                known_limitations: vec![],
                verification_notes: vec!["The pinned test.sh command checks the candidate".into()],
            })
            .map(|payload| ("ImplementationHandoff", payload))?,
            "reviewer" => {
                ensure!(
                    input_handoff.is_some_and(|handoff| {
                        handoff.handoff_type == HandoffType::Implementation
                            && handoff.workspace_state_id.as_deref()
                                == wf_run.current_workspace_state_id.as_deref()
                    }),
                    "reviewer did not receive the implementation handoff for the current candidate"
                );
                serde_json::to_value(ReviewDecision {
                    decision: ReviewDecisionStatus::Approve,
                    summary: "Reviewed the exact candidate diff and its deterministic check".into(),
                    findings: vec![],
                    requested_changes: vec![],
                    suggested_additional_checks: vec![],
                })
                .map(|payload| ("ReviewDecision", payload))?
            }
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

        drop(peer_wire);
        drop(orbit_wire);
        let cleanup_confirmed = true;

        self.callbacks
            .lock()
            .expect("offline ACP callback audit mutex poisoned")
            .push(OfflineRoleCallbacks {
                role_id: role.role_id.clone(),
                target: target.clone(),
                workspace_state_id: wf_run.current_workspace_state_id.clone(),
                input_handoff_type: input_handoff.map(|handoff| handoff.handoff_type),
                input_handoff_workspace_state_id: input_handoff
                    .and_then(|handoff| handoff.workspace_state_id.clone()),
                read_response,
                write_responses,
                diff_response,
                provider_reply_type: provider_reply_type.into(),
                cleanup_confirmed,
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
        match role.role_id.as_str() {
            "planner" => ensure!(
                input_handoff.is_none(),
                "planner received an unexpected handoff"
            ),
            "implementer" => ensure!(
                input_handoff.is_some_and(|handoff| handoff.handoff_type == HandoffType::Plan),
                "implementer did not receive its durable plan"
            ),
            "reviewer" => ensure!(
                input_handoff.is_some_and(|handoff| {
                    handoff.handoff_type == HandoffType::Implementation
                        && handoff.workspace_state_id.as_deref()
                            == wf_run.current_workspace_state_id.as_deref()
                }),
                "reviewer did not receive the implementation handoff for the current candidate"
            ),
            other => anyhow::bail!("unexpected role in offline ACP executor: {other}"),
        }

        let raw_output = self
            .transport
            .run_role(OfflineRoleContext {
                pool,
                wf_run,
                role_exec,
                role,
                target,
                repo_path,
                input_handoff,
            })
            .await?;
        Ok(RoleExecutionOutcome {
            raw_output,
            agent_execution_ids: vec![],
            termination_reason: Some("deterministic offline ACP peer; endpoints dropped".into()),
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
async fn b32_07_full_coordinator_offline_acp_callbacks_handoffs_and_verification() -> Result<()> {
    let ctx = setup_test().await?;
    enroll_sample_credentials(&ctx.engine.pool).await?;

    let (repo, baseline_revision) = create_full_coordinator_fixture_repo()?;
    let repo_path = repo.path().to_string_lossy().into_owned();
    let mut verification_policy = VerificationPolicy::new(
        "b32-offline-full-verification",
        "Offline full coordinator candidate contract",
    );
    verification_policy.required_steps = vec!["candidate-contract".into()];
    verification_policy.allowed_commands =
        vec![AllowedCommand::with_prefix("sh", vec!["test.sh".into()])];
    verification_policy.network_policy = VerificationNetworkPolicy::None;
    verification_policy.cache_policy = VerificationCachePolicy::Clean;

    let mut selection_policy = SelectionPolicy::new(
        "b32-offline-full-selection",
        "Offline full coordinator selection",
    );
    let mut candidate_check = VerificationCheck::new_command(
        "candidate-contract",
        "Check the exact candidate content",
        vec![
            VerificationTier::Fast,
            VerificationTier::Standard,
            VerificationTier::Full,
        ],
        vec!["sh".into(), "test.sh".into()],
    );
    candidate_check.always_run = true;
    selection_policy.checks.push(candidate_check);

    let mut regression_policy = RegressionPolicy::new(
        "b32-offline-full-regression",
        "Offline full coordinator regression",
    );
    regression_policy.fallback_behavior = RegressionFallbackBehavior::FailClosed;
    regression_policy.selection_policy_id = Some(selection_policy.id.clone());
    regression_policy.selection_policy_version = Some(selection_policy.version);
    regression_policy.selection_policy_digest = Some(selection_policy.digest());

    ctx.store
        .verification_store()
        .save_policy(&verification_policy)
        .await?;
    let regression_store = RegressionStore::new(ctx.engine.pool.clone());
    regression_store
        .insert_selection_policy(&selection_policy)
        .await?;
    regression_store
        .insert_regression_policy(&regression_policy)
        .await?;

    let attempt_id = format!("att-{}", id());
    let wf = ctx
        .store
        .create_workflow_run_full(
            "b32-offline-acp",
            &attempt_id,
            2,
            Some(&verification_policy),
            Some(&regression_policy),
            Some(&selection_policy),
            Some("Update the fixture candidate marker and make test.sh assert it"),
            Some(&repo_path),
            Some(&baseline_revision),
        )
        .await?;

    let transport = Arc::new(DeterministicAcpTransport::default());
    let executor = Arc::new(OfflineAcpRoleExecutor::new(transport.clone()));
    let verification_environment = pinned_test_verification_environment().await?;
    let coordinator = WorkflowCoordinator::new(ctx.engine.pool.clone(), executor)
        .with_verification_environment(verification_environment.clone())?;

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

    let implemented = ctx
        .store
        .get_workflow_run(&wf.id)
        .await?
        .context("workflow disappeared after implementation")?;
    assert_eq!(implemented.status, WorkflowStage::Verifying);
    let reviewed_workspace_state_id = implemented
        .current_workspace_state_id
        .clone()
        .context("implementer did not persist a workspace state")?;

    let plan_artifact = ctx
        .store
        .get_latest_handoff_of_type(&wf.id, HandoffType::Plan)
        .await?
        .context("coordinator did not persist the planner handoff")?;
    let plan: PlanHandoff = serde_json::from_value(plan_artifact.structured_payload)?;
    assert_eq!(plan.summary, "Inspected the offline candidate contract");

    let implementation_artifact = ctx
        .store
        .get_latest_handoff_of_type(&wf.id, HandoffType::Implementation)
        .await?
        .context("coordinator did not persist the implementation handoff")?;
    assert_eq!(
        implementation_artifact.workspace_state_id.as_deref(),
        Some(reviewed_workspace_state_id.as_str())
    );
    let implementation: ImplementationHandoff =
        serde_json::from_value(implementation_artifact.structured_payload)?;
    assert_eq!(
        implementation.summary,
        "Updated the candidate and its deterministic check"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("README.md"))?,
        "offline fixture candidate: full-coordinator\n"
    );
    assert!(
        std::fs::read_to_string(repo.path().join("test.sh"))?.contains("offline full check passed")
    );

    {
        let callback_evidence = transport
            .callbacks
            .lock()
            .expect("offline ACP callback audit mutex poisoned");
        assert_eq!(
            callback_evidence.len(),
            2,
            "planner and implementer coordinator turns ran before verification"
        );
        assert_eq!(callback_evidence[0].role_id, "planner");
        assert_eq!(callback_evidence[1].role_id, "implementer");
        for evidence in callback_evidence.iter() {
            assert!(evidence.cleanup_confirmed);
            assert!(
                evidence.read_response["result"]["content"]
                    .as_str()
                    .is_some_and(|content| match evidence.role_id.as_str() {
                        "reviewer" => {
                            content.contains("offline fixture candidate: full-coordinator")
                        }
                        _ => content.contains("offline fixture baseline"),
                    }),
                "the ACP read callback must reach repository tools"
            );
            assert!(evidence.provider_reply_type.ends_with("Handoff"));
            if evidence.role_id == "planner" {
                assert_eq!(evidence.tool_calls, 2);
                assert_eq!(evidence.tool_successes, 1);
                assert_eq!(evidence.tool_failures, 1);
            } else if evidence.role_id == "implementer" {
                assert_eq!(evidence.tool_calls, 3);
                assert_eq!(evidence.tool_successes, 3);
                assert_eq!(evidence.tool_failures, 0);
            }
        }
        assert!(
            callback_evidence[0].write_responses[0]["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains(orbit::tool_surface::ERR_READ_ONLY_ROLE)),
            "planner mutation must be denied by role authorization"
        );
        for response in &callback_evidence[1].write_responses {
            assert!(
                response.get("result").is_some(),
                "implementer with the persisted role execution lock must be allowed"
            );
        }
    }

    assert_eq!(
        coordinator.step(&wf.id).await?,
        WorkflowStepResult::Advanced {
            from: WorkflowStage::Verifying,
            to: WorkflowStage::Reviewing,
        }
    );
    let verification_runs = coordinator
        .verification_store()
        .list_runs(&attempt_id)
        .await?;
    for tier in [VerificationTier::Fast, VerificationTier::Standard] {
        let run = verification_runs
            .iter()
            .find(|run| run.tier == Some(tier))
            .with_context(|| format!("coordinator did not persist {tier} evidence"))?;
        assert_eq!(run.workspace_state_id, reviewed_workspace_state_id);
        assert_eq!(run.overall_result, Some(VerificationRunResult::Passed));
        assert_eq!(
            run.environment_identity.runtime_image,
            verification_environment.runtime_image
        );
        assert_eq!(
            run.environment_identity.runtime_image_digest,
            verification_environment.runtime_image_digest
        );
        assert_eq!(
            run.environment_identity.isolation,
            verification_environment.isolation
        );
        assert_eq!(
            run.environment_identity.network_policy,
            VerificationNetworkPolicy::None
        );
        assert!(run.step_runs.iter().any(|step| {
            step.step_id == "candidate-contract"
                && step.status == VerificationStepStatus::Passed
                && step
                    .stdout_preview
                    .as_deref()
                    .is_some_and(|stdout| stdout.contains("offline full check passed"))
        }));
    }

    assert_eq!(
        coordinator.step(&wf.id).await?,
        WorkflowStepResult::Advanced {
            from: WorkflowStage::Reviewing,
            to: WorkflowStage::Regression,
        }
    );
    let reviewer_artifact = ctx
        .store
        .get_latest_handoff_of_type(&wf.id, HandoffType::Review)
        .await?
        .context("coordinator did not persist reviewer decision")?;
    assert_eq!(
        reviewer_artifact.workspace_state_id.as_deref(),
        Some(reviewed_workspace_state_id.as_str()),
        "review approval must bind to the exact candidate state"
    );
    let review: ReviewDecision = serde_json::from_value(reviewer_artifact.structured_payload)?;
    assert_eq!(review.decision, ReviewDecisionStatus::Approve);
    assert!(review.summary.contains("exact candidate diff"));

    {
        let callback_evidence = transport
            .callbacks
            .lock()
            .expect("offline ACP callback audit mutex poisoned");
        assert_eq!(callback_evidence.len(), 3);
        let reviewer = callback_evidence
            .iter()
            .find(|evidence| evidence.role_id == "reviewer")
            .context("reviewer coordinator role execution was not run")?;
        assert!(reviewer.cleanup_confirmed);
        assert_eq!(
            reviewer.workspace_state_id.as_deref(),
            Some(reviewed_workspace_state_id.as_str())
        );
        assert_eq!(
            reviewer.input_handoff_type,
            Some(HandoffType::Implementation)
        );
        assert_eq!(
            reviewer.input_handoff_workspace_state_id.as_deref(),
            Some(reviewed_workspace_state_id.as_str())
        );
        assert_eq!(reviewer.provider_reply_type, "ReviewDecision");
        let diff = reviewer
            .diff_response
            .as_ref()
            .and_then(|response| response["result"]["diff"].as_str())
            .context("reviewer did not receive candidate diff through ACP callback")?;
        assert!(diff.contains("offline fixture candidate: full-coordinator"));
        assert!(diff.contains("offline full check passed"));
        assert!(
            reviewer.write_responses[0]["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains(orbit::tool_surface::ERR_READ_ONLY_ROLE))
        );
        assert_eq!(reviewer.tool_calls, 3);
        assert_eq!(reviewer.tool_successes, 2);
        assert_eq!(reviewer.tool_failures, 1);
    }

    assert_eq!(
        coordinator.step(&wf.id).await?,
        WorkflowStepResult::Terminal(WorkflowStage::Completed)
    );
    let completed = ctx
        .store
        .get_workflow_run(&wf.id)
        .await?
        .context("completed workflow disappeared")?;
    assert_eq!(completed.status, WorkflowStage::Completed);
    assert_eq!(
        completed.verification_policy_id.as_deref(),
        Some(verification_policy.id.as_str()),
        "workflow completion must retain the pinned verification policy"
    );
    let final_workspace_state_id = completed
        .current_workspace_state_id
        .as_deref()
        .context("completed workflow has no final workspace state")?;
    assert_eq!(final_workspace_state_id, reviewed_workspace_state_id);

    let final_verification_runs = coordinator
        .verification_store()
        .list_runs(&attempt_id)
        .await?;
    let full_run = final_verification_runs
        .iter()
        .find(|run| run.tier == Some(VerificationTier::Full))
        .context("coordinator did not persist FULL regression evidence")?;
    assert_eq!(full_run.workspace_state_id, reviewed_workspace_state_id);
    assert_eq!(full_run.overall_result, Some(VerificationRunResult::Passed));
    assert_eq!(full_run.policy_id.as_deref(), Some("policy-full"));
    assert_eq!(full_run.plan_snapshot.id, "plan-full");
    assert_eq!(
        full_run.regression_policy_id.as_deref(),
        Some(regression_policy.id.as_str())
    );
    let full_selection_id = full_run
        .selection_id
        .as_deref()
        .context("FULL regression run has no durable verification selection")?;
    let full_selection = coordinator
        .regression_store()
        .get_selection(full_selection_id)
        .await?
        .context("FULL verification selection was not durably persisted")?;
    assert_eq!(full_selection.requested_tier, VerificationTier::Full);
    assert_eq!(
        full_selection.workspace_state_id, reviewed_workspace_state_id,
        "FULL selection must bind to the reviewer-approved candidate"
    );
    assert_eq!(
        full_selection.selection_policy_id.as_deref(),
        Some(selection_policy.id.as_str())
    );
    assert!(
        full_selection
            .selected_checks
            .iter()
            .any(|check| check.check_id == "candidate-contract")
    );
    assert_eq!(
        full_run.selection_digest.as_deref(),
        Some(full_selection.digest.as_str())
    );
    assert_eq!(full_run.plan_snapshot.steps.len(), 1);
    assert_eq!(full_run.plan_snapshot.steps[0].argv, ["sh", "test.sh"]);
    assert_eq!(full_run.step_runs.len(), 1);
    assert_eq!(full_run.step_runs[0].step_id, "candidate-contract");
    assert_eq!(full_run.step_runs[0].status, VerificationStepStatus::Passed);
    assert!(
        full_run.step_runs[0]
            .stdout_preview
            .as_deref()
            .is_some_and(|stdout| stdout.contains(
                "offline full check passed: offline fixture candidate: full-coordinator"
            ))
    );

    let role_executions = ctx.store.list_role_executions(&wf.id).await?;
    assert_eq!(role_executions.len(), 3);
    for expected_role in ["planner", "implementer", "reviewer"] {
        let execution = role_executions
            .iter()
            .find(|execution| execution.role_id == expected_role)
            .with_context(|| format!("missing {expected_role} role execution"))?;
        assert_eq!(execution.status, RoleExecutionStatus::Succeeded);
        assert!(execution.finished_at_ms.is_some());
        let target = execution
            .resolved_target
            .as_ref()
            .with_context(|| format!("{expected_role} has no durable resolved target"))?;
        let expected = match expected_role {
            "planner" | "implementer" => ("codex", "codex-main", "gpt-6-luna"),
            "reviewer" => ("antigravity", "antigravity-ch9b2013", "gemini-3.8-flash"),
            _ => unreachable!(),
        };
        assert_eq!(target.provider, expected.0);
        assert_eq!(target.credential_id.as_deref(), Some(expected.1));
        assert_eq!(target.resolved_model.as_deref(), Some(expected.2));
        assert!(target.resolution_reason.contains("reset-aware rank=1"));
    }

    {
        let callback_evidence = transport
            .callbacks
            .lock()
            .expect("offline ACP callback audit mutex poisoned");
        assert_eq!(callback_evidence.len(), role_executions.len());
        for evidence in callback_evidence.iter() {
            assert!(evidence.cleanup_confirmed);
            let role_execution = role_executions
                .iter()
                .find(|execution| execution.role_id == evidence.role_id)
                .expect("callback has a durable role execution");
            assert_eq!(
                Some(&evidence.target),
                role_execution.resolved_target.as_ref()
            );
        }
    }

    let active_lock_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM orbit_attempt_workspace_locks WHERE attempt_id = $1 AND revoked_at_ms IS NULL",
    )
    .bind(&attempt_id)
    .fetch_one(&ctx.engine.pool)
    .await?;
    assert_eq!(
        active_lock_count, 0,
        "completed attempt retained a mutation lock"
    );
    let step_owner: Option<String> =
        sqlx::query_scalar("SELECT step_owner_id FROM orbit_workflow_runs WHERE id = $1")
            .bind(&wf.id)
            .fetch_one(&ctx.engine.pool)
            .await?;
    assert!(
        step_owner.is_none(),
        "completed workflow retained a step owner"
    );

    teardown_test(ctx).await?;
    Ok(())
}

const TEST_VERIFICATION_IMAGE_REF: &str = "docker.io/library/alpine@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b";

async fn pinned_test_verification_environment() -> Result<EnvironmentIdentity> {
    let image = tokio::process::Command::new("podman")
        .args([
            "--remote=false",
            "image",
            "inspect",
            TEST_VERIFICATION_IMAGE_REF,
            "--format",
            "{{.Id}}",
        ])
        .output()
        .await
        .context("cannot inspect the pinned rootless Podman verification image")?;
    ensure!(
        image.status.success(),
        "pinned rootless Podman verification image is not available locally"
    );
    let image_id = String::from_utf8(image.stdout)?.trim().to_owned();
    ensure!(
        image_id.len() == 64 && image_id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Podman returned an invalid local image ID for the pinned verification image"
    );

    Ok(EnvironmentIdentity {
        execution_profile: "sandboxed-container".into(),
        isolation: "rootless-podman".into(),
        runtime_image: Some(TEST_VERIFICATION_IMAGE_REF.into()),
        runtime_image_digest: Some(format!("sha256:{image_id}")),
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
    })
}

fn create_full_coordinator_fixture_repo() -> Result<(tempfile::TempDir, String)> {
    let repo = tempfile::tempdir().context("create offline coordinator repository")?;
    common::init_git_repo(repo.path())?;
    std::fs::write(repo.path().join("README.md"), "offline fixture baseline\n")?;
    std::fs::write(
        repo.path().join("test.sh"),
        "#!/bin/sh\nset -eu\ntest \"$(cat README.md)\" = \"offline fixture baseline\"\nprintf 'offline baseline check passed\\n'\n",
    )?;
    fixture_git(repo.path(), &["add", "README.md", "test.sh"])?;
    fixture_git(
        repo.path(),
        &["commit", "--quiet", "-m", "fixture baseline"],
    )?;
    let baseline_revision = fixture_git(repo.path(), &["rev-parse", "HEAD"])?;
    Ok((repo, baseline_revision))
}

fn fixture_git(repo_path: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(repo_path)
        .output()
        .with_context(|| format!("failed to start git {args:?}"))?;
    ensure!(
        output.status.success(),
        "git {args:?} failed with status {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}
