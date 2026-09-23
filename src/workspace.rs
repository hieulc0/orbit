//! Disposable OCI repository execution, supervised separately from the lease owner.
use crate::{
    execution::{Profile, ValidatorRequirement, validate_tool_command},
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
    if step.uses == "repository.test" {
        return perform_validation(client, a, directory, home, profile).await;
    }
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
                    let failure = coding_runtime_failure(&error);
                    let diagnostic = coding_runtime_failure_log(&failure)?;
                    let logs = client.upload(a, "logs", diagnostic).await?;
                    return Ok((false, vec![logs], Some(failure)));
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
        anyhow::bail!("unsupported workspace capability");
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

fn coding_runtime_failure(error: &anyhow::Error) -> Failure {
    Failure {
        category: "infrastructure_failure".into(),
        code: if error.is::<crate::acp_runtime::TurnTimeout>() {
            "turn_timeout"
        } else {
            "coding_agent_failed"
        }
        .into(),
        message: "coding runtime failed; inspect invocation receipts and logs".into(),
        // The engine independently overrides this from unresolved reservations.
        side_effect_status: "none".into(),
    }
}

fn coding_runtime_failure_log(failure: &Failure) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&json!({
        "kind": "coding_runtime_failure",
        "category": failure.category,
        "code": failure.code,
        "side_effect_status": failure.side_effect_status,
    }))?)
}

const VALIDATION_LOG_LIMIT: usize = 1024 * 1024;
const VALIDATION_RECORD_LIMIT: usize = 256;

fn epoch_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Default)]
struct ValidationEvidence {
    commands: Vec<Value>,
    logs: Vec<u8>,
    logs_truncated: bool,
    failure: Option<Failure>,
}

fn bounded_text(text: &str, limit: usize) -> &str {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

impl ValidationEvidence {
    fn log(&mut self, bytes: &[u8]) {
        let keep = bytes.len().min(VALIDATION_LOG_LIMIT - self.logs.len());
        self.logs.extend_from_slice(&bytes[..keep]);
        self.logs_truncated |= keep < bytes.len();
    }

    fn fail(&mut self, infrastructure: bool, code: &str, message: &str) {
        self.failure = Some(Failure {
            category: if infrastructure {
                "infrastructure_failure"
            } else {
                "task_failure"
            }
            .into(),
            code: code.into(),
            message: bounded_text(message, 4096).into(),
            side_effect_status: "none".into(),
        });
        self.log(bounded_text(message, 4096).as_bytes());
        self.log(b"\n");
    }

    async fn run<F, Fut>(
        &mut self,
        phase: &str,
        validator: usize,
        command: CommandSpec,
        execute: &mut F,
    ) -> Result<bool>
    where
        F: FnMut(CommandSpec) -> Fut,
        Fut: std::future::Future<Output = Result<(Option<i32>, Vec<u8>, Vec<u8>, bool)>>,
    {
        ensure!(
            self.commands.len() < VALIDATION_RECORD_LIMIT,
            "validation report command limit exceeded"
        );
        validate_tool_command(&command)?;
        let mut remaining = 4096;
        let argv: Vec<_> = command
            .argv
            .iter()
            .map(|arg| {
                let kept = bounded_text(arg, remaining);
                remaining -= kept.len();
                kept
            })
            .collect();
        let mut record = json!({"phase":phase,"validator_index":validator,
            "argv":argv,"argv_truncated":argv != command.argv,
            "cwd":bounded_text(&command.cwd, 1024),
            "command_digest":digest(&serde_json::to_vec(&command)?), "started_at":epoch_millis()});
        let result = execute(command).await;
        record["finished_at"] = json!(epoch_millis());
        match result {
            Ok((code, out, err, timeout)) => {
                self.log(&out);
                self.log(&err);
                record["exit_code"] = json!(code);
                record["timed_out"] = json!(timeout);
                let passed = code == Some(0) && !timeout;
                record["success"] = json!(passed);
                self.commands.push(record);
                if !passed {
                    let preflight = phase == "preflight";
                    // Podman's reserved launch statuses are infrastructure failures.
                    // Do not infer missing components from language-specific stderr.
                    let infrastructure = preflight
                        || matches!(code, Some(125..=127))
                        || (code.is_none() && !timeout);
                    self.fail(infrastructure,
                        if preflight { "validation_preflight_failed" }
                        else if infrastructure { "validation_infrastructure_failed" }
                        else { "validation_failed" },
                        &format!("{phase} for validator {validator}: exit={code:?}; timed_out={timeout}; see validation artifacts"));
                }
                Ok(passed)
            }
            Err(_) => {
                record["success"] = json!(false);
                record["error"] = json!("validator execution or cleanup unconfirmed");
                self.commands.push(record);
                self.fail(
                    true,
                    "validation_infrastructure_failed",
                    "validator execution or cleanup unconfirmed",
                );
                Ok(false)
            }
        }
    }
}

async fn validation_commands<F, Fut>(
    commands: &[CommandSpec],
    requirements: &[ValidatorRequirement],
    evidence: &mut ValidationEvidence,
    mut execute: F,
) -> Result<()>
where
    F: FnMut(CommandSpec) -> Fut,
    Fut: std::future::Future<Output = Result<(Option<i32>, Vec<u8>, Vec<u8>, bool)>>,
{
    // Check all requirements before running any validator. Probe in the same
    // candidate cwd so repository-selected toolchains are checked too.
    for (index, command) in commands.iter().enumerate() {
        validate_tool_command(command)?;
        let lookup = CommandSpec {
            argv: vec!["/bin/sh".into(), "-c".into(),
                "command -v -- \"$1\" >/dev/null 2>&1 || { printf 'validator executable unavailable: %s\\n' \"$1\" >&2; exit 127; }".into(),
                "orbit-validator-preflight".into(), command.argv[0].clone()],
            cwd: command.cwd.clone(),
            timeout_seconds: command.timeout_seconds.min(30),
        };
        if !evidence
            .run("preflight", index, lookup, &mut execute)
            .await?
        {
            return Ok(());
        }
        for requirement in requirements
            .iter()
            .filter(|r| command.argv.starts_with(&r.command_prefix))
        {
            for argv in &requirement.probes {
                let probe = CommandSpec {
                    argv: argv.clone(),
                    cwd: command.cwd.clone(),
                    timeout_seconds: command.timeout_seconds.min(30),
                };
                if !evidence
                    .run("preflight", index, probe, &mut execute)
                    .await?
                {
                    return Ok(());
                }
            }
        }
    }
    for (index, command) in commands.iter().enumerate() {
        if !evidence
            .run("validate", index, command.clone(), &mut execute)
            .await?
        {
            break;
        }
    }
    Ok(())
}

async fn perform_validation(
    client: &Client,
    a: &Assignment,
    directory: &Path,
    home: &Path,
    profile: &Profile,
) -> Result<(bool, Vec<String>, Option<Failure>)> {
    let config = client
        .execution_config
        .as_ref()
        .context("execution configuration missing")?;
    let patch = a
        .input_artifacts
        .iter()
        .find(|artifact| artifact.kind == "patch");
    let mut evidence = ValidationEvidence::default();
    let result: Result<()> = async {
        let workspace = Workspace::materialize(a, directory, &config.credentials).await?;
        let patch = patch.context("accepted patch missing")?;
        let bytes = client.artifact(&a.run_id, patch).await?;
        let patch_path = directory.join("input.patch");
        tokio::fs::write(&patch_path, &bytes).await?;
        if !bytes.is_empty()
            && workspace
                .git(&[
                    "apply",
                    "--index",
                    patch_path.to_str().context("invalid patch path")?,
                ])
                .await
                .is_err()
        {
            evidence
                .commands
                .push(json!({"phase":"apply_patch","success":false}));
            evidence.fail(
                false,
                "validation_failed",
                "accepted patch could not be applied to the pinned baseline",
            );
            return Ok(());
        }
        evidence
            .commands
            .push(json!({"phase":"apply_patch","success":true}));
        let commands = a.plan.definition.steps[&a.step]
            .commands
            .as_ref()
            .context("test commands missing")?;
        validation_commands(
                commands,
                &config.validator_requirements,
                &mut evidence,
                |command| {
                    let workspace = &workspace.path;
                    async move {
                        execute_validation(a, workspace, directory, home, profile, &command).await
                    }
                },
            )
            .await
    }
    .await;
    if result.is_err() {
        evidence.fail(
            true,
            "validation_infrastructure_failed",
            "validation setup/execution failed; inspect worker and runtime health",
        );
    }
    let success = evidence.failure.is_none();
    let test_report = json!({"patch_id":patch.map(|p| &p.id),"patch_checksum":patch.map(|p| &p.checksum),
        "base_revision":a.plan.definition.inputs.base_revision,"attempt_id":a.attempt_id,
        "success":success,"failure":evidence.failure,"commands":evidence.commands,"profile":profile,
        "logs_truncated":evidence.logs_truncated});
    // All validation outcomes share the normal fenced publication protocol.
    // Storage/lease failures must still propagate; they cannot authorize artifacts.
    let mut artifacts = vec![
        client
            .upload(a, "test_report", serde_json::to_vec(&test_report)?)
            .await?,
    ];
    artifacts.push(client.upload(a, "logs", evidence.logs).await?);
    artifacts.push(
        client
            .upload(
                a,
                "execution_report",
                serde_json::to_vec(&report(a, profile))?,
            )
            .await?,
    );
    Ok((success, artifacts, evidence.failure))
}

async fn execute_validation(
    a: &Assignment,
    workspace: &Path,
    directory: &Path,
    home: &Path,
    profile: &Profile,
    command: &CommandSpec,
) -> Result<(Option<i32>, Vec<u8>, Vec<u8>, bool)> {
    let spec = prepare_execution(a, workspace, directory, profile, command).await?;
    let result = crate::worker::supervised_command(&spec, directory, home, a).await?;
    // A supervisor's own failure must not look like a validator's exit status.
    // The receipt exists only after runtime cleanup has completed.
    let code = crate::acp_process::read_cleanup(Path::new(&spec.argv[3]), Some(&a.attempt_id))
        .context("validator supervisor did not confirm container cleanup")?;
    ensure!(
        result.0 == Some(code) && !result.3,
        "validator supervisor exit disagrees with cleanup receipt"
    );
    Ok((result.0, result.1, result.2, code == 124))
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

#[cfg(test)]
mod validation_tests {
    use super::*;

    fn command() -> CommandSpec {
        CommandSpec {
            argv: vec!["compiler".into(), "test".into()],
            cwd: ".".into(),
            timeout_seconds: 20,
        }
    }

    #[test]
    fn prompt_timeout_classification_survives_cleanup_context() {
        let timeout = anyhow::Error::new(crate::acp_runtime::TurnTimeout {
            diagnostic: "bounded protocol state".into(),
        })
        .context("cleanup confirmed");
        assert_eq!(coding_runtime_failure(&timeout).code, "turn_timeout");
        let launch = coding_runtime_failure(&anyhow::anyhow!("runtime failed"));
        assert_eq!(launch.code, "coding_agent_failed");
        assert_eq!(launch.category, "infrastructure_failure");
    }

    #[test]
    fn coding_runtime_failure_log_is_structural() -> Result<()> {
        let failure = coding_runtime_failure(&anyhow::anyhow!(
            "Authorization: Bearer ORBIT_SECRET_SENTINEL"
        ));
        let log = String::from_utf8(coding_runtime_failure_log(&failure)?)?;
        assert!(!log.contains("ORBIT_SECRET_SENTINEL"));
        assert!(log.contains("coding_agent_failed"));
        Ok(())
    }

    #[tokio::test]
    async fn missing_validator_is_infrastructure_and_never_runs_validation() -> Result<()> {
        let mut evidence = ValidationEvidence::default();
        let mut calls = 0;
        validation_commands(&[command()], &[], &mut evidence, |_| {
            calls += 1;
            std::future::ready(Ok((Some(127), vec![], b"unavailable".to_vec(), false)))
        })
        .await?;
        assert_eq!(calls, 1);
        assert_eq!(
            evidence.failure.as_ref().unwrap().category,
            "infrastructure_failure"
        );
        assert_eq!(
            evidence.failure.as_ref().unwrap().code,
            "validation_preflight_failed"
        );
        assert_eq!(evidence.commands[0]["phase"], "preflight");
        assert!(evidence.commands[0]["started_at"].as_u64().is_some());
        assert!(
            evidence.commands[0]["finished_at"].as_u64().unwrap()
                >= evidence.commands[0]["started_at"].as_u64().unwrap()
        );
        Ok(())
    }

    #[tokio::test]
    async fn component_probe_failure_is_not_generated_code_failure() -> Result<()> {
        let requirement = ValidatorRequirement {
            command_prefix: vec!["compiler".into()],
            probes: vec![vec!["compiler".into(), "--version".into()]],
        };
        let mut evidence = ValidationEvidence::default();
        let mut calls = 0;
        validation_commands(&[command()], &[requirement], &mut evidence, |_| {
            calls += 1;
            std::future::ready(Ok((
                Some(if calls == 1 { 0 } else { 1 }),
                vec![],
                vec![],
                false,
            )))
        })
        .await?;
        assert_eq!(calls, 2);
        assert_eq!(evidence.failure.unwrap().category, "infrastructure_failure");
        assert!(evidence.commands.iter().all(|r| r["phase"] == "preflight"));
        Ok(())
    }

    #[tokio::test]
    async fn compile_failure_after_preflight_is_code_failure() -> Result<()> {
        let mut evidence = ValidationEvidence::default();
        let mut calls = 0;
        validation_commands(&[command()], &[], &mut evidence, |_| {
            calls += 1;
            std::future::ready(Ok((
                Some(if calls == 1 { 0 } else { 1 }),
                vec![],
                b"compile failed".to_vec(),
                false,
            )))
        })
        .await?;
        assert_eq!(evidence.failure.unwrap().category, "task_failure");
        assert_eq!(evidence.commands[1]["phase"], "validate");
        assert_eq!(evidence.commands[1]["exit_code"], 1);
        Ok(())
    }

    #[tokio::test]
    async fn validation_diagnostics_are_bounded_and_supervisor_errors_sanitized() -> Result<()> {
        let mut evidence = ValidationEvidence::default();
        evidence.log(&vec![b'x'; VALIDATION_LOG_LIMIT + 100]);
        assert_eq!(evidence.logs.len(), VALIDATION_LOG_LIMIT);
        assert!(evidence.logs_truncated);
        evidence
            .run("validate", 0, command(), &mut |_| {
                std::future::ready(Err(anyhow::anyhow!("Authorization: Bearer secret")))
            })
            .await?;
        assert_eq!(
            evidence.failure.as_ref().unwrap().category,
            "infrastructure_failure"
        );
        assert!(!serde_json::to_string(&evidence.commands)?.contains("secret"));
        assert!(!evidence.failure.unwrap().message.contains("secret"));
        Ok(())
    }

    #[tokio::test]
    async fn validation_success_requires_actual_checks_after_all_probes() -> Result<()> {
        let mut evidence = ValidationEvidence::default();
        validation_commands(&[command(), command()], &[], &mut evidence, |_| {
            std::future::ready(Ok((Some(0), vec![], vec![], false)))
        })
        .await?;
        assert!(evidence.failure.is_none());
        assert_eq!(
            evidence
                .commands
                .iter()
                .map(|r| r["phase"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["preflight", "preflight", "validate", "validate"]
        );
        Ok(())
    }
}
