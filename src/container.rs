//! Local OCI adapters. A separate supervisor owns cleanup when its worker dies.
use crate::{model::*, worker::Client};
use anyhow::{Context, Result, ensure};
use serde_json::json;
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};

pub fn runtime() -> Result<String> {
    let runtime = std::env::var("ORBIT_CONTAINER_RUNTIME").unwrap_or_else(|_| "docker".into());
    ensure!(
        ["docker", "podman"].contains(&runtime.as_str()),
        "ORBIT_CONTAINER_RUNTIME must be docker or podman"
    );
    Ok(runtime)
}

pub async fn perform(
    client: &Client,
    a: &Assignment,
    directory: &Path,
    home: &Path,
) -> Result<(bool, Vec<String>, Option<Failure>)> {
    let step = &a.plan.definition.steps[&a.step];
    let spec = step
        .container
        .as_ref()
        .context("container configuration missing")?;
    spec.validate()?;
    let inputs = directory.join("inputs");
    let outputs = directory.join("outputs");
    tokio::fs::create_dir(&inputs).await?;
    tokio::fs::create_dir(&outputs).await?;
    for artifact in &a.input_artifacts {
        uuid::Uuid::parse_str(&artifact.id)?;
        tokio::fs::write(
            inputs.join(&artifact.id),
            client.artifact(&a.run_id, artifact).await?,
        )
        .await?;
    }
    tokio::fs::write(
        inputs.join("manifest.json"),
        serde_json::to_vec(&a.input_artifacts)?,
    )
    .await?;
    let command = CommandSpec {
        argv: vec![
            std::env::current_exe()?.to_string_lossy().into_owned(),
            "container-supervisor".into(),
            "--assignment".into(),
            directory
                .join("assignment.json")
                .to_string_lossy()
                .into_owned(),
        ],
        cwd: ".".into(),
        timeout_seconds: step.timeout_seconds,
    };
    let (code, stdout, stderr, timed_out) =
        crate::worker::supervised_command(&command, directory, home, a).await?;
    let success = code == Some(0) && !timed_out;
    let logs = client.upload(a, "logs", [stdout, stderr].concat()).await?;
    let report = client
        .upload(
            a,
            "container_report",
            serde_json::to_vec(&json!({
                "attempt_id":a.attempt_id, "idempotency_key":a.idempotency_key,
                "image":spec.image, "resources":step.resources, "success":success,
                "exit_code":code, "timed_out":timed_out,
            }))?,
        )
        .await?;
    let mut artifacts = vec![logs, report];
    let result = outputs.join("result");
    if success {
        match tokio::fs::symlink_metadata(&result).await {
            Ok(metadata) => {
                ensure!(
                    metadata.is_file()
                        && !metadata.file_type().is_symlink()
                        && metadata.len() <= crate::artifacts::MAX_ARTIFACT_BYTES,
                    "container result must be a regular file of at most 32 MiB"
                );
                use tokio::io::AsyncReadExt;
                let mut options = tokio::fs::OpenOptions::new();
                options.read(true);
                #[cfg(unix)]
                options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
                let file = options.open(result).await?;
                ensure!(
                    file.metadata().await?.is_file(),
                    "container result must be a regular file"
                );
                let mut bytes = vec![];
                file.take(crate::artifacts::MAX_ARTIFACT_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .await?;
                ensure!(
                    bytes.len() as u64 <= crate::artifacts::MAX_ARTIFACT_BYTES,
                    "container result too large"
                );
                artifacts.push(client.upload(a, "data", bytes).await?);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok((
        success,
        artifacts,
        (!success).then(|| Failure {
            category: "task_failure".into(),
            code: "container_failed".into(),
            message: "container command failed; see container report and logs".into(),
            side_effect_status: "none".into(),
        }),
    ))
}

/// The parent holds stdin open until execution ends. EOF, timeout, and normal exit
/// all converge on removing this attempt's container, even after parent SIGKILL.
pub async fn supervise(assignment_path: &Path) -> Result<i32> {
    let a: Assignment = serde_json::from_slice(&tokio::fs::read(assignment_path).await?)?;
    uuid::Uuid::parse_str(&a.attempt_id)?;
    a.plan.definition.validate()?;
    let step = a
        .plan
        .definition
        .steps
        .get(&a.step)
        .context("container step missing")?;
    ensure!(
        step.uses == "container.run",
        "supervisor requires container.run"
    );
    let spec = step
        .container
        .as_ref()
        .context("container configuration missing")?;
    let resources = step
        .resources
        .as_ref()
        .context("container resources missing")?;
    let directory = assignment_path
        .parent()
        .context("assignment directory missing")?
        .canonicalize()?;
    let name = format!("orbit-{}", a.attempt_id);
    let input_path = directory.join("inputs");
    let output_path = directory.join("outputs");
    ensure!(
        !directory.to_string_lossy().contains(','),
        "container workspace path cannot contain commas"
    );
    let runtime = runtime()?;
    let mut command = Command::new(&runtime);
    if runtime == "podman" {
        command.arg("--cgroup-manager=cgroupfs");
    }
    command.args([
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
        "--tmpfs",
        "/tmp:rw,noexec,nosuid,size=16777216",
        "--log-driver=none",
        "--cpus",
        &format!("{:.3}", resources.cpu_millis as f64 / 1000.),
        "--memory",
        &format!("{}m", resources.memory_mib),
        "--memory-swap",
        &format!("{}m", resources.memory_mib),
        "--mount",
        &format!(
            "type=bind,src={},dst=/orbit/inputs,readonly",
            input_path.display()
        ),
        "--mount",
        &format!("type=bind,src={},dst=/orbit/outputs", output_path.display()),
        "--env",
        &format!("ORBIT_TASK_ID={}", a.task_id),
        "--env",
        &format!("ORBIT_ATTEMPT_ID={}", a.attempt_id),
        "--env",
        &format!("ORBIT_ATTEMPT_GENERATION={}", a.generation),
        "--env",
        &format!("ORBIT_IDEMPOTENCY_KEY={}", a.idempotency_key),
    ]);
    #[cfg(unix)]
    command.args([
        "--user",
        &format!("{}:{}", unsafe { libc::getuid() }, unsafe {
            libc::getgid()
        }),
    ]);
    if runtime == "podman" {
        command.arg("--userns=keep-id");
    }
    if resources.gpu > 0 {
        ensure!(
            a.gpu_devices.len() == resources.gpu as usize,
            "assigned GPU devices missing"
        );
        let devices = a
            .gpu_devices
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        if runtime == "podman" {
            for device in &a.gpu_devices {
                command.args(["--device", &format!("nvidia.com/gpu={device}")]);
            }
        } else {
            command.args(["--gpus", &format!("\"device={devices}\"")]);
        }
    }
    command
        .args(["--entrypoint", &spec.command[0], &spec.image])
        .args(&spec.command[1..]);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    run_supervised(command, &runtime, &name, step.timeout_seconds).await
}

pub(crate) async fn run_supervised(
    mut command: Command,
    runtime: &str,
    name: &str,
    timeout: u64,
) -> Result<i32> {
    let mut child = command
        .spawn()
        .context("cannot start container runtime; provision runtime and pinned image")?;
    let disconnected = async {
        let mut stdin = tokio::io::stdin();
        let mut byte = [0];
        loop {
            if stdin.read(&mut byte).await? == 0 {
                return Ok::<(), std::io::Error>(());
            }
        }
    };
    let result = tokio::select! {
        result = child.wait() => result.map(|s| s.code().unwrap_or(1)),
        result = disconnected => result.map(|_| 125),
        _ = tokio::time::sleep(Duration::from_secs(timeout)) => Ok(124),
    };
    let _ = child.kill().await;
    // Docker may finish a create after its CLI has disconnected. Retry cleanup
    // for a bounded grace period; report daemon errors without claiming a stop.
    let mut stopped = false;
    for _ in 0..10 {
        let mut cleanup = Command::new(runtime);
        if runtime == "podman" {
            cleanup.args(["--remote=false", "--cgroup-manager=cgroupfs"]);
        }
        let removed = tokio::time::timeout(
            Duration::from_secs(5),
            cleanup
                .args(["rm", "--force", name])
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await;
        if let Ok(Ok(output)) = removed {
            if output.status.success() {
                stopped = true;
                break;
            }
            if String::from_utf8_lossy(&output.stderr).contains("No such container") {
                stopped = true;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    ensure!(
        stopped,
        "container cleanup unconfirmed; inspect attempt container {name}"
    );
    Ok(result?)
}
