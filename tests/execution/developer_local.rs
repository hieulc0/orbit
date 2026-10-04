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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    // Positive control: the host endpoint exists before testing that the
    // terminal cannot reach it from its separate network namespace.
    let connection = tokio::net::TcpStream::connect(address).await?;
    drop(connection);
    std::fs::write(workspace.path().join(".git"), "protected metadata")?;
    std::os::unix::fs::symlink(host.path(), workspace.path().join("escape"))?;
    let profile = RoleExecutionProfile::DevLocal {
        bubblewrap: PathBuf::from("/usr/bin/bwrap"),
    };
    let command = format!(
        "test ! -e '{}' && test ! -e escape/secret && test ! -e /home/orbit/.ssh && test ! -e /home/orbit/.aws && test ! -e /home/orbit/.orbit/private && test ! -e /run/podman/podman.sock && test ! -e /var/run/docker.sock && test -z \"${{ORBIT_TERMINAL_SECRET:-}}\" && test \"$(ls /sys/class/net 2>/dev/null | wc -l)\" -eq 0 && ! printf changed > .git && ! rm .git && command -v timeout >/dev/null && command -v bash >/dev/null && ! timeout 2 bash -c 'exec 3<>/dev/tcp/127.0.0.1/{}' && printf local > candidate.txt",
        host.path().display(),
        address.port()
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
        store.set_role_execution_resolved(&role.id, &ResolvedExecutionTarget { provider:"fixture".into(),runtime_interface:"fixture".into(), credential_id:None,credential_generation:None,requested_model:None,resolved_model:None,runtime_image_digest:None,admitted_runtime:None,resolution_reason:"offline fixture".into() }).await?;
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

/// Compare only command isolation overhead. Provider runtimes remain OCI in
/// both profiles, and repository callbacks are already native in both.
#[tokio::test]
#[ignore = "requires bubblewrap and an explicitly provisioned pinned OCI compiler image"]
async fn confined_command_latency_against_rootless_oci() -> Result<()> {
    use anyhow::Context;
    use serde_json::json;
    use std::time::Instant;
    let image = std::env::var("ORBIT_TEST_COMMAND_IMAGE")
        .context("select a provisioned image by digest")?;
    let (_, digest) = image
        .rsplit_once("@sha256:")
        .context("image must be pinned")?;
    ensure!(
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid image digest"
    );
    let workspace = tempfile::tempdir()?;
    std::fs::write(
        workspace.path().join("calc.c"),
        "int main(void) { return 3 * 4 == 12 ? 0 : 1; }\n",
    )?;
    let profile = RoleExecutionProfile::DevLocal {
        bubblewrap: PathBuf::from("/usr/bin/bwrap"),
    };
    for (label, script) in [
        ("startup", "true"),
        ("compile_and_run", "cc calc.c -o /tmp/calc && /tmp/calc"),
    ] {
        let mut local = Vec::new();
        let mut oci = Vec::new();
        // Alternate order to reduce the influence of host warming. Keep the
        // warm-up separate; every measured command starts a fresh boundary.
        for sample in 0..8 {
            for native in if sample % 2 == 0 {
                [true, false]
            } else {
                [false, true]
            } {
                let started = Instant::now();
                if native {
                    let terminal = AgentTerminal::spawn_confined(
                        &profile,
                        workspace.path(),
                        workspace.path(),
                        "sh",
                        &["-c".into(), script.into()],
                        4096,
                    )?;
                    ensure!(
                        terminal.wait_for_exit(Duration::from_secs(30)).await? == 0,
                        "local workload failed: {}",
                        terminal.output().text()
                    );
                    terminal.kill().await?;
                } else {
                    let name = format!("orbit-command-comparison-{}", orbit::model::id());
                    let mut command = tokio::process::Command::new("podman");
                    command
                        .args([
                            "--remote=false",
                            "--cgroup-manager=cgroupfs",
                            "run",
                            "--rm",
                            "--pull=never",
                            "--name",
                            &name,
                            "--network=none",
                            "--read-only",
                            "--cap-drop=ALL",
                            "--security-opt=no-new-privileges",
                            "--pids-limit=64",
                            "--memory=512m",
                            "--cpus=1",
                            "--userns=keep-id",
                            "--tmpfs=/tmp:rw,nosuid,nodev,size=64m",
                            "--workdir=/workspace",
                            "--volume",
                        ])
                        .arg(format!("{}:/workspace:rw", workspace.path().display()))
                        .args([&image, "sh", "-c", script]);
                    command.kill_on_drop(true);
                    let output =
                        tokio::time::timeout(Duration::from_secs(30), command.output()).await;
                    if output.is_err() {
                        let _ = tokio::process::Command::new("podman")
                            .args(["--remote=false", "rm", "--force", &name])
                            .output()
                            .await;
                    }
                    let output = output??;
                    ensure!(
                        output.status.success(),
                        "OCI workload failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                if sample > 0 {
                    if native {
                        local.push(started.elapsed().as_secs_f64() * 1000.0);
                    } else {
                        oci.push(started.elapsed().as_secs_f64() * 1000.0);
                    }
                }
            }
        }
        local.sort_by(f64::total_cmp);
        oci.sort_by(f64::total_cmp);
        println!(
            "COMMAND_LATENCY {}",
            json!({"workload":label,"samples":7,"dev_local_ms":local,"rootless_oci_ms":oci,"dev_local_median_ms":local[3],"rootless_oci_median_ms":oci[3],"oci_image":image})
        );
        ensure!(
            local[3] < oci[3],
            "no measured local latency improvement for {label}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn native_repository_operations_preserve_candidate_confinement() -> Result<()> {
    use orbit::{
        execution::worktree::ManagedWorktree,
        fs_tools::read_text_confined,
        tool_surface::{git_diff, git_status, search_grep},
        workflow_coordinator::compute_workspace_state,
    };
    use std::time::Instant;
    let source = common::TemporaryGitRepo::create()?;
    let root = tempfile::tempdir()?;
    let candidate = ManagedWorktree::create(source.path(), &root.path().join("candidate")).await?;
    let outside = tempfile::tempdir()?;
    std::fs::write(outside.path().join("secret"), "host secret")?;
    std::os::unix::fs::symlink(outside.path(), candidate.workspace.join("outside"))?;
    let started = Instant::now();
    ensure!(
        read_text_confined(&candidate.workspace, "README.md")?.contains("offline fixture"),
        "native read failed"
    );
    ensure!(
        read_text_confined(&candidate.workspace, "outside/secret").is_err(),
        "native read escaped candidate"
    );
    let read_ms = started.elapsed().as_secs_f64() * 1000.0;
    let started = Instant::now();
    ensure!(
        !search_grep(
            &candidate.workspace,
            Some("README.md"),
            "offline",
            true,
            false,
            &[],
            &[],
            10,
            0
        )?
        .matches
        .is_empty(),
        "native search failed"
    );
    let search_ms = started.elapsed().as_secs_f64() * 1000.0;
    std::fs::write(candidate.workspace.join("README.md"), "changed candidate\n")?;
    let started = Instant::now();
    ensure!(
        git_status(&candidate.workspace, None)
            .await?
            .modified
            .contains(&"README.md".into()),
        "native Git status failed on linked worktree"
    );
    ensure!(
        git_diff(&candidate.workspace, None, None, None, false, 4096)
            .await?
            .diff
            .contains("changed candidate"),
        "native Git diff failed"
    );
    let git_ms = started.elapsed().as_secs_f64() * 1000.0;
    std::fs::remove_file(candidate.workspace.join("outside"))?;
    let started = Instant::now();
    let state = compute_workspace_state(&candidate.workspace, &candidate.base_revision).await?;
    let state_ms = started.elapsed().as_secs_f64() * 1000.0;
    println!(
        "NATIVE_REPOSITORY_LATENCY {}",
        serde_json::json!({"read_ms":read_ms,"search_ms":search_ms,"git_status_diff_ms":git_ms,"workspace_state_ms":state_ms})
    );
    ensure!(
        std::fs::read_to_string(source.path().join("README.md"))? == "offline fixture baseline\n",
        "main checkout changed"
    );
    candidate.discard(&state.state_id).await?;
    Ok(())
}
