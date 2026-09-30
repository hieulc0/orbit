use anyhow::{Result, ensure};
use orbit::{
    execution::local::RoleExecutionProfile, tool_surface::AgentTerminal, workflow::WorkflowStore,
};
use std::{path::PathBuf, time::Duration};

#[path = "../common/mod.rs"]
#[allow(dead_code)]
mod common;

#[tokio::test]
#[ignore = "requires Linux user namespaces and operator-provisioned bubblewrap"]
async fn confined_terminal_cannot_observe_host_state() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let host = tempfile::tempdir()?;
    std::fs::write(host.path().join("secret"), "host-only")?;
    std::fs::write(workspace.path().join(".git"), "protected metadata")?;
    std::os::unix::fs::symlink(host.path(), workspace.path().join("escape"))?;
    let profile = RoleExecutionProfile::DevLocal {
        bubblewrap: PathBuf::from("/usr/bin/bwrap"),
    };
    let command = format!(
        "test ! -e '{}' && test ! -e escape/secret && test ! -e /home/orbit/.ssh && test ! -e /home/orbit/.aws && test ! -e /home/orbit/.orbit/private && test ! -e /run/podman/podman.sock && test ! -e /var/run/docker.sock && test -z \"${{ORBIT_TERMINAL_SECRET:-}}\" && test \"$(ls /sys/class/net 2>/dev/null | wc -l)\" -eq 0 && ! printf changed > .git && printf local > candidate.txt",
        host.path().display()
    );
    let terminal = AgentTerminal::spawn_confined(
        &profile,
        workspace.path(),
        workspace.path(),
        "sh",
        &["-c".into(), command],
        4096,
    )?;
    let code = terminal.wait_for_exit(Duration::from_secs(10)).await?;
    ensure!(
        code == 0,
        "local confinement failed: {}",
        terminal.output().text()
    );
    ensure!(
        std::fs::read_to_string(workspace.path().join("candidate.txt"))? == "local",
        "workspace not writable"
    );
    ensure!(
        std::fs::read_to_string(workspace.path().join(".git"))? == "protected metadata",
        "metadata modified"
    );
    terminal.kill().await?;
    let running = AgentTerminal::spawn_confined(
        &profile,
        workspace.path(),
        workspace.path(),
        "sh",
        &["-c".into(), "sleep 60 & wait".into()],
        1024,
    )?;
    running.kill().await?;
    ensure!(
        running.output().exit_code.is_some(),
        "terminal cleanup unconfirmed"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable ORBIT_TEST_DATABASE_URL"]
async fn profile_is_immutable_and_cannot_change_after_claim() -> Result<()> {
    let database = common::DisposablePgTestContext::create("execution_profile", 3).await?;
    let result = async {
        let store = WorkflowStore::new(database.engine.pool.clone());
        let run = store
            .create_workflow_run_full(
                "task-local",
                "attempt-local",
                3,
                None,
                None,
                None,
                Some("local fixture"),
                None,
                None,
            )
            .await?;
        ensure!(
            store.execution_profile(&run.id).await? == RoleExecutionProfile::Trusted,
            "legacy profile changed"
        );
        let local = RoleExecutionProfile::DevLocal {
            bubblewrap: PathBuf::from("/usr/bin/bwrap"),
        };
        store.pin_execution_profile(&run.id, &local).await?;
        store.pin_execution_profile(&run.id, &local).await?;
        ensure!(
            store
                .pin_execution_profile(&run.id, &RoleExecutionProfile::Trusted)
                .await
                .is_err(),
            "profile overwritten"
        );
        ensure!(
            store.execution_profile(&run.id).await? == local,
            "stored profile changed"
        );
        let claim = store.claim_workflow_step(&run.id).await?.unwrap();
        ensure!(
            store.pin_execution_profile(&run.id, &local).await.is_err(),
            "profile changed under active claim"
        );
        store.release_workflow_step(&claim).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    database.teardown().await?;
    result
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL and Linux user namespaces"]
async fn atomic_shell_has_one_fenced_callback_and_confirms_cleanup() -> Result<()> {
    use orbit::{
        acp_wire::{OrbitToolInvocationMeta, Wire},
        workflow::{ResolvedExecutionTarget, RoleDefinition, WorkspaceAccess},
        workflow_coordinator::{AcpTurnState, handle_acp_message},
    };
    use serde_json::json;
    let database = common::DisposablePgTestContext::create("local_shell", 3).await?;
    let result = async {
        let repository = tempfile::tempdir()?;
        let canonical = repository.path().canonicalize()?;
        let store = WorkflowStore::new(database.engine.pool.clone());
        let run = store.create_workflow_run_full("task-shell", "attempt-shell", 3, None, None, None, Some("shell fixture"), Some(canonical.to_str().unwrap()), None).await?;
        let role = store.create_role_execution(&run.id, &RoleDefinition::implementer_v1(), "IMPLEMENTING", 0, None, None).await?;
        store.set_role_execution_resolved(&role.id, &ResolvedExecutionTarget { provider:"fixture".into(),runtime_interface:"fixture".into(), credential_id:None,credential_generation:None,requested_model:None,resolved_model:None,runtime_image_digest:None,resolution_reason:"offline fixture".into() }).await?;
        store.start_agent_execution("agent-shell", &role.id, "fixture", None, None, 0, None, None, &json!({})).await?;
        store.acquire_workspace_mutation_lock(&run.attempt_id, &role.id).await?;
        let (client_input, server_output) = tokio::io::duplex(65536);
        let (server_input, client_output) = tokio::io::duplex(65536);
        let mut server = Wire::new(server_input, server_output, 1024 * 1024);
        let mut client = Wire::new(client_input, client_output, 1024 * 1024);
        let mut state = AcpTurnState::new(&canonical, WorkspaceAccess::ReadWrite);
        state.role_id = Some("implementer".into());
        state.workspace_identity = Some(canonical.to_string_lossy().into_owned());
        state.execution_profile = RoleExecutionProfile::DevLocal {bubblewrap:PathBuf::from("/usr/bin/bwrap")};
        state.role_budget = Some(orbit::tools::budget::RoleBudget::for_role("implementer"));
        state.wf_attempt_id = Some(run.attempt_id.clone());
        state.role_exec_id = Some(role.id.clone());
        state.agent_exec_id = Some("agent-shell".into());
        state.pool = Some(&database.engine.pool);
        let invocation = OrbitToolInvocationMeta::new("inv-shell", "provider-shell")?;
        handle_acp_message(&mut server, &mut state, json!({"method":"session/update","params":{"update":{"sessionUpdate":"tool_call","toolCallId":"provider-shell","title":"orbit_shell","kind":"execute","status":"in_progress"}},"_meta":invocation.envelope_metadata()})).await?;
        handle_acp_message(&mut server, &mut state, json!({"id":"callback-shell","method":"orbit/shell","params":{"command":"sh","args":["-c","printf candidate > result.txt; printf ready"]},"_meta":invocation.envelope_metadata()})).await?;
        let response = client.read().await?;
        ensure!(response["result"]["exit_code"] == 0 && response["result"]["output"] == "ready", "confined callback failed: {response}");
        ensure!(state.terminals.is_empty() && state.tool_calls == 1 && state.tool_successes == 1 && state.tool_failures == 0 && state.role_usage.terminal_calls == 1, "callback or cleanup accounting failed");
        let metadata: serde_json::Value = sqlx::query_scalar("SELECT metadata FROM orbit_agent_executions WHERE id = 'agent-shell'").fetch_one(&database.engine.pool).await?;
        let audit = &metadata["tool_call_audit"];
        ensure!(audit["entries"][0]["provider_update_correlation"] == "CORRELATED" && audit["entries"][0]["terminal_state"] == "SUCCESS" && audit["summary"]["unmatched_callbacks"] == 0, "shell correlation missing: {audit}");
        ensure!(std::fs::read_to_string(canonical.join("result.txt"))? == "candidate", "mutation missing");
        store.release_workspace_mutation_lock(&run.attempt_id, &role.id).await?;
        Ok::<_, anyhow::Error>(())
    }.await;
    database.teardown().await?;
    result
}
