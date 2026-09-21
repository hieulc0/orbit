//! Disposable OCI repository execution, supervised separately from the lease owner.
use crate::{
    execution::{Profile, validate_tool_command},
    model::*,
    repository::Workspace,
    worker::Client,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::process::Command;

pub async fn perform(
    client: &Client,
    a: &Assignment,
    directory: &Path,
    home: &Path,
) -> Result<(bool, Vec<String>, Option<Failure>)> {
    let config = client
        .execution_config
        .as_ref()
        .context("configure --execution-config for workspace execution")?;
    let profile = config.authorize(a)?;
    let step = &a.plan.definition.steps[&a.step];
    let acp_agent = step.agent.as_ref().and_then(|spec| {
        config
            .acp_agents
            .iter()
            .find(|agent| agent.binding_name == spec.binding)
    });
    if let Some(agent) = acp_agent {
        agent.authorize(a)?;
    } else if step.agent.is_some() {
        config
            .coding_agent
            .as_ref()
            .context("coding runtime unavailable")?
            .authorize(a)?;
    }
    let workspace = Workspace::materialize(a, directory, &config.credentials).await?;
    let mut artifacts = vec![];
    let mut logs = Vec::new();
    let mut failure: Option<Failure> = None;
    let success;
    if step.uses == "repository.code" {
        if step.agent.is_some() {
            let session = crate::coding_agent::Session {
                client,
                assignment: a,
                workspace: &workspace,
                directory,
                home,
                profile,
                credentials: &config.credentials,
            };
            let result = if let Some(agent) = acp_agent {
                agent.run(session).await
            } else {
                config
                    .coding_agent
                    .as_ref()
                    .context("coding runtime unavailable")?
                    .run(session)
                    .await
            };
            let (report, agent_logs) = match result {
                Ok(result) => result,
                Err(error) => {
                    let logs = client
                        .upload(a, "logs", error.to_string().into_bytes())
                        .await?;
                    return Ok((
                        false,
                        vec![logs],
                        Some(Failure {
                            category: "task_failure".into(),
                            code: "coding_agent_failed".into(),
                            message: "coding runtime failed; inspect invocation receipts and logs"
                                .into(),
                            side_effect_status: "none".into(),
                        }),
                    ));
                }
            };
            logs = agent_logs;
            artifacts.push(
                client
                    .upload(a, "agent_report", serde_json::to_vec(&report)?)
                    .await?,
            );
            if report
                .output
                .get("acp")
                .and_then(|a| a.get("stop_reason"))
                .and_then(|s| s.as_str())
                == Some("budget_exhausted")
            {
                success = false;
                failure = Some(Failure {
                    category: "task_failure".into(),
                    code: "budget_exhausted".into(),
                    message: "Orbit call budget exhausted".into(),
                    side_effect_status: "none".into(),
                });
            } else {
                success = true;
            }
        } else {
            ensure!(step.agent.is_none(), "coding runtime unavailable");
            let result = execute(
                a,
                &workspace.path,
                directory,
                home,
                profile,
                &a.plan.repository.coding_command,
            )
            .await?;
            success = result.0 == Some(0) && !result.3;
            logs.extend(result.1);
            logs.extend(result.2);
        }
        if success {
            let (patch, manifest) = workspace.patch(a).await?;
            artifacts.push(client.upload(a, "patch", patch).await?);
            artifacts.push(client.upload(a, "manifest", manifest).await?);
        }
    } else {
        ensure!(
            step.uses == "repository.test",
            "unsupported workspace capability"
        );
        let patch = a
            .input_artifacts
            .iter()
            .find(|a| a.kind == "patch")
            .context("accepted patch missing")?;
        let bytes = client.artifact(&a.run_id, patch).await?;
        let patch_path = directory.join("input.patch");
        tokio::fs::write(&patch_path, &bytes).await?;
        let applied = bytes.is_empty()
            || workspace
                .git(&[
                    "apply",
                    "--index",
                    patch_path.to_str().context("invalid patch path")?,
                ])
                .await
                .is_ok();
        let mut reports = vec![json!({"phase":"apply_patch","success":applied})];
        let mut passed = applied;
        if applied {
            for cmd in step.commands.as_ref().context("test commands missing")? {
                let (code, out, err, timeout) =
                    execute(a, &workspace.path, directory, home, profile, cmd).await?;
                ensure!(
                    logs.len() + out.len() + err.len()
                        <= crate::artifacts::MAX_ARTIFACT_BYTES as usize,
                    "test logs exceed artifact limit"
                );
                logs.extend(out);
                logs.extend(err);
                reports.push(
                    json!({"argv":cmd.argv,"cwd":cmd.cwd,"exit_code":code,"timed_out":timeout}),
                );
                if code != Some(0) || timeout {
                    passed = false;
                    break;
                }
            }
        }
        success = passed;
        artifacts.push(client.upload(a, "test_report", serde_json::to_vec(&json!({"patch_id":patch.id,"patch_checksum":patch.checksum,
            "base_revision":a.plan.definition.inputs.base_revision,"attempt_id":a.attempt_id,"success":success,"commands":reports}))?).await?);
    }
    artifacts.push(client.upload(a, "logs", logs).await?);
    artifacts.push(
        client
            .upload(
                a,
                "execution_report",
                serde_json::to_vec(&report(a, profile))?,
            )
            .await?,
    );
    Ok((
        success,
        artifacts,
        failure.or_else(|| {
            (!success).then(|| Failure {
                category: "task_failure".into(),
                code: "validation_failed".into(),
                message: "workspace command or independent verification failed; see artifacts"
                    .into(),
                side_effect_status: "none".into(),
            })
        }),
    ))
}

pub fn report(a: &Assignment, profile: &Profile) -> Value {
    json!({"attempt_id":a.attempt_id,"plan_digest":a.plan.digest,"profile":profile,
        "requirements":a.plan.definition.steps[&a.step].execution,"resources":a.plan.definition.steps[&a.step].resources})
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    assignment: Assignment,
    workspace: PathBuf,
    profile: Profile,
    command: CommandSpec,
    invocation_id: String,
}

pub async fn execute(
    a: &Assignment,
    workspace: &Path,
    directory: &Path,
    home: &Path,
    profile: &Profile,
    command: &CommandSpec,
) -> Result<(Option<i32>, Vec<u8>, Vec<u8>, bool)> {
    let spec = prepare_execution(a, workspace, directory, profile, command).await?;
    crate::worker::supervised_command(&spec, directory, home, a).await
}

pub(crate) async fn prepare_execution(
    a: &Assignment,
    workspace: &Path,
    directory: &Path,
    profile: &Profile,
    command: &CommandSpec,
) -> Result<CommandSpec> {
    validate_tool_command(command)?;
    let invocation = id();
    let request_path = directory.join(format!("exec-{invocation}.json"));
    let mut assignment = a.clone();
    assignment.lease_token.clear();
    let request = Request {
        assignment,
        workspace: workspace.to_owned(),
        profile: profile.clone(),
        command: command.clone(),
        invocation_id: invocation,
    };
    tokio::fs::write(&request_path, serde_json::to_vec(&request)?).await?;
    let spec = CommandSpec {
        argv: vec![
            crate::worker::current_executable()?
                .to_string_lossy()
                .into(),
            "workspace-supervisor".into(),
            "--request".into(),
            request_path.to_string_lossy().into(),
        ],
        cwd: ".".into(),
        timeout_seconds: (command.timeout_seconds + 60).min(604800),
    };
    Ok(spec)
}

pub async fn supervise(path: &Path) -> Result<i32> {
    ensure!(
        unsafe { libc::getuid() } != 0,
        "workspace execution requires a non-root worker"
    );
    let request: Request = serde_json::from_slice(&tokio::fs::read(path).await?)?;
    uuid::Uuid::parse_str(&request.invocation_id)?;
    let a = &request.assignment;
    a.plan.definition.validate()?;
    let step = &a.plan.definition.steps[&a.step];
    let requirements = step
        .execution
        .as_ref()
        .context("execution requirements missing")?;
    request.profile.validate(&requirements.isolation)?;
    ensure!(
        a.plan.execution_profiles.get(&requirements.isolation) == Some(&request.profile),
        "pinned execution profile mismatch"
    );
    validate_tool_command(&request.command)?;
    let root = path
        .parent()
        .context("request directory missing")?
        .canonicalize()?;
    let workspace = request.workspace.canonicalize()?;
    ensure!(
        workspace == root.join("repository") && !workspace.to_string_lossy().contains(','),
        "workspace mount must be attempt-owned"
    );
    // Resolve cwd inside the container, so a task-controlled symlink is never
    // followed by the privileged supervisor on the host.
    let cwd = format!("/workspace/{}", request.command.cwd);
    let mut resources = step
        .resources
        .as_ref()
        .context("execution resources missing")?
        .clone();
    if step
        .agent
        .as_ref()
        .is_some_and(|agent| agent.acp_limits.is_some())
    {
        // The other half is reserved for the isolated agent process. Never
        // overbook scheduler capacity with a second full-size tool container.
        resources.cpu_millis /= 2;
        resources.memory_mib /= 2;
        ensure!(
            resources.cpu_millis >= 100 && resources.memory_mib >= 64,
            "ACP terminal resources too small"
        );
    }
    let name = format!("orbit-{}-{}", a.attempt_id, request.invocation_id);
    let mut cmd = Command::new("podman");
    cmd.args([
        "--remote=false",
        "--cgroup-manager=cgroupfs",
        "run",
        "--pull=never",
        "--name",
        &name,
        "--label",
        "orbit.managed=true",
        "--label",
        &format!("orbit.attempt={}", a.attempt_id),
        "--network=none",
        "--read-only",
        "--cap-drop=ALL",
        "--security-opt=no-new-privileges",
        "--pids-limit=128",
        "--init",
        "--log-driver=none",
        "--userns=keep-id",
        "--user",
        &format!("{}:{}", unsafe { libc::getuid() }, unsafe {
            libc::getgid()
        }),
        "--cpus",
        &format!("{:.3}", resources.cpu_millis as f64 / 1000.0),
        "--memory",
        &format!("{}m", resources.memory_mib),
        "--memory-swap",
        &format!("{}m", resources.memory_mib),
        "--tmpfs",
        "/tmp:rw,nosuid,nodev,size=67108864",
        "--mount",
        &format!("type=bind,src={},dst=/workspace", workspace.display()),
        "--workdir",
        &cwd,
        "--env",
        "HOME=/tmp",
        "--env",
        "GIT_CONFIG_NOSYSTEM=1",
        "--env",
        "GIT_CONFIG_GLOBAL=/dev/null",
        "--env",
        &format!("ORBIT_TASK_ID={}", a.task_id),
        "--env",
        &format!("ORBIT_ATTEMPT_ID={}", a.attempt_id),
        "--entrypoint",
        &request.command.argv[0],
        &request.profile.image,
    ])
    .args(&request.command.argv[1..])
    .stdin(Stdio::null())
    .stdout(Stdio::inherit())
    .stderr(Stdio::inherit())
    .kill_on_drop(true);
    let code = crate::container::run_supervised(
        cmd,
        "podman",
        &name,
        request.command.timeout_seconds.min(step.timeout_seconds),
    )
    .await?;
    crate::acp_process::write_cleanup(path, &a.attempt_id, code)?;
    Ok(code)
}
