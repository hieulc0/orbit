//! Independent OCI agent supervisor with a worker stdio lifeline and auth quarantine.
use crate::{
    acp_files::Root,
    acp_runtime::{Adapter, AgentNetwork, Launch, Runtime},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{ChildStderr, Command},
};

const MAX_LAUNCH_DIAGNOSTIC_BYTES: usize = 8192;
// JSON escaping can expand bounded diagnostics several times. Both readers
// accept the same bounded receipt written by the supervisor.
const MAX_CLEANUP_RECEIPT_BYTES: usize = 65536;
const MAX_LIFECYCLE_OBSERVATION_BYTES: usize = 8192;

/// A private supervisor receipt is written only after all container effects stop.
/// It is not an engine receipt; the worker still submits a fenced call receipt.
pub fn write_cleanup(request: &Path, attempt: &str, code: i32) -> Result<()> {
    write_cleanup_diagnostic(request, attempt, code, "unknown", None, None)
}

pub fn write_cleanup_diagnostic(
    request: &Path,
    attempt: &str,
    code: i32,
    launch_stage: &str,
    image: Option<&str>,
    diagnostic: Option<&str>,
) -> Result<()> {
    write_cleanup_receipt(
        request,
        attempt,
        code,
        launch_stage,
        image,
        DiagnosticStatus {
            present: diagnostic.is_some(),
            truncated: false,
        },
        None,
    )
}

struct DiagnosticStatus {
    present: bool,
    truncated: bool,
}

fn write_cleanup_receipt(
    request: &Path,
    attempt: &str,
    code: i32,
    launch_stage: &str,
    image: Option<&str>,
    diagnostic: DiagnosticStatus,
    observation: Option<serde_json::Value>,
) -> Result<()> {
    use std::io::Write;
    if let Some(observation) = &observation {
        ensure!(
            serde_json::to_vec(observation)?.len() <= MAX_LIFECYCLE_OBSERVATION_BYTES,
            "ACP lifecycle observation too large"
        );
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(request.with_extension("cleanup.json"))?;
    file.write_all(&serde_json::to_vec(&serde_json::json!({
        "format":"orbit-process-cleanup/v4",
        "request":request.file_name().and_then(|s|s.to_str()).context("invalid cleanup request filename")?,
        "attempt_id":attempt,
        "exit_code":code,
        "runtime":"podman",
        "launch_stage":launch_stage,
        "image":image,
        // Child stdout/stderr is untrusted content. Persist only structural
        // facts about its presence and bounded capture.
        "diagnostic_present":diagnostic.present,
        "diagnostic_truncated":diagnostic.truncated,
        "observation":observation
    }))?)?;
    file.sync_all()?;
    Ok(())
}

pub fn read_cleanup(request: &Path, attempt: Option<&str>) -> Result<i32> {
    let root = Root::open(request.parent().context("cleanup directory missing")?)?;
    let receipt = request.with_extension("cleanup.json");
    let bytes = root.read_private(
        receipt
            .file_name()
            .unwrap()
            .to_str()
            .context("invalid receipt path")?,
        MAX_CLEANUP_RECEIPT_BYTES,
    )?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    ensure!(
        (value["format"] == "orbit-process-cleanup/v1"
            || value["format"] == "orbit-process-cleanup/v2"
            || value["format"] == "orbit-process-cleanup/v3"
            || value["format"] == "orbit-process-cleanup/v4")
            && value["request"] == request.file_name().unwrap().to_str().unwrap()
            && attempt.is_none_or(|id| value["attempt_id"] == id),
        "foreign cleanup receipt"
    );
    Ok(i32::try_from(
        value["exit_code"]
            .as_i64()
            .context("cleanup exit missing")?,
    )?)
}

pub fn read_cleanup_diagnostic(request: &Path, attempt: Option<&str>) -> Result<Option<String>> {
    let root = Root::open(request.parent().context("cleanup directory missing")?)?;
    let receipt = request.with_extension("cleanup.json");
    let bytes = root.read_private(
        receipt
            .file_name()
            .unwrap()
            .to_str()
            .context("invalid receipt path")?,
        MAX_CLEANUP_RECEIPT_BYTES,
    )?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    ensure!(
        (value["format"] == "orbit-process-cleanup/v1"
            || value["format"] == "orbit-process-cleanup/v2"
            || value["format"] == "orbit-process-cleanup/v3"
            || value["format"] == "orbit-process-cleanup/v4")
            && value["request"] == request.file_name().unwrap().to_str().unwrap()
            && attempt.is_none_or(|id| value["attempt_id"] == id),
        "foreign cleanup receipt"
    );
    if value.get("launch_stage").is_none()
        && value.get("runtime").is_none()
        && value.get("image").is_none()
        && value.get("diagnostic").is_none()
        && value.get("diagnostic_present").is_none()
    {
        return Ok(None);
    }
    let mut message = format!(
        "runtime={} image={} stage={} exit_code={}",
        value["runtime"].as_str().unwrap_or("unknown"),
        value["image"].as_str().unwrap_or("unknown"),
        value["launch_stage"].as_str().unwrap_or("unknown"),
        value["exit_code"].as_i64().unwrap_or(-1)
    );
    // Preserve legacy receipt deserialization, but never re-expose historical
    // child text through the current inspection surface. New v4 receipts carry
    // the same fact explicitly and never contain the text at all.
    let diagnostic_present = if value["format"] == "orbit-process-cleanup/v4" {
        value["diagnostic_present"].as_bool().unwrap_or(false)
    } else {
        value["diagnostic"].is_string()
    };
    if diagnostic_present {
        message.push_str(" diagnostic_present=true");
        if value["format"] == "orbit-process-cleanup/v4"
            && value["diagnostic_truncated"].as_bool().unwrap_or(false)
        {
            message.push_str(" diagnostic_truncated=true");
        }
    }
    if !value["observation"].is_null() {
        message.push_str(" observation=");
        message.push_str(&serde_json::to_string(&value["observation"])?);
    }
    Ok(Some(message))
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub runtime: Runtime,
    pub attempt_id: String,
    pub timeout_seconds: u64,
    pub tools: Vec<String>,
}

pub struct AuthLease {
    _lock: File,
    root: Root,
    path: PathBuf,
    files: std::collections::BTreeMap<String, String>,
}
impl AuthLease {
    pub fn acquire(runtime: &Runtime) -> Result<Self> {
        runtime.validate()?;
        let path = &runtime.auth.path;
        ensure!(
            path.canonicalize()? == *path
                && std::fs::metadata(path)?.permissions().mode() & 0o077 == 0,
            "ACP auth store must be a canonical private directory"
        );
        let root = Root::open(path)?;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(path.join(".orbit-acp.lock"))?;
        ensure!(
            lock.metadata()?.is_file()
                && lock.metadata()?.permissions().mode() & 0o077 == 0
                && lock.metadata()?.nlink() == 1
                && lock.metadata()?.uid() == unsafe { libc::geteuid() },
            "invalid ACP auth lock"
        );
        ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "ACP auth store is in use"
        );
        ensure!(
            !path.join(".orbit-acp-active.json").try_exists()?,
            "ACP auth store quarantined; inspect and stop the recorded container before clearing its marker"
        );
        for source in runtime.auth.files.keys() {
            root.read_private(source, 65536)?;
        }
        Ok(Self {
            _lock: lock,
            root,
            path: path.clone(),
            files: runtime.auth.files.clone(),
        })
    }
    pub fn stage(&self, home: &Path, container: &str, attempt: &str) -> Result<()> {
        ensure!(!home.exists(), "ACP control directory must be new");
        std::fs::create_dir(home)?;
        std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700))?;
        std::fs::create_dir(home.join("workspace"))?;
        for (source, destination) in &self.files {
            let destination = home.join(destination);
            std::fs::create_dir_all(destination.parent().unwrap())?;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(destination)?;
            use std::io::Write;
            file.write_all(&self.root.read_private(source, 65536)?)?;
            file.sync_all()?;
        }
        let mut marker = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.path.join(".orbit-acp-active.json"))?;
        use std::io::Write;
        marker.write_all(&serde_json::to_vec(&serde_json::json!({"container":container,"attempt_id":attempt,"format":"orbit-acp-auth-lease/v1"}))?)?;
        marker.sync_all()?;
        Ok(())
    }
    /// Call only after confirmed container removal. On any failure the marker
    /// remains, preventing another worker from reusing uncertain auth state.
    pub fn finish(self, home: &Path) -> Result<()> {
        let control = Root::open(home)?;
        for (source, destination) in &self.files {
            let bytes = control.read_private(destination, 65536)?;
            self.root.replace_private(source, &bytes)?;
            control.write(destination, &[])?;
        }
        // Credential staging files are cleared through confined fds. Keep the
        // private control directory for reviewed retention; never mount it
        // into a repository tool or export it as an artifact.
        std::fs::remove_file(self.path.join(".orbit-acp-active.json"))?;
        Ok(())
    }
}

pub fn command(request: &Request, home: &Path, name: &str, image: &str) -> Result<Command> {
    request.runtime.validate()?;
    ensure!(
        unsafe { libc::getuid() } != 0,
        "ACP supervision requires rootless Podman"
    );
    ensure!(
        !home.to_string_lossy().contains(','),
        "ACP control path cannot contain commas"
    );
    let launch = &request.runtime.launch;
    let mut command = Command::new("podman");
    command
        .args([
            "--remote=false",
            "--cgroup-manager=cgroupfs",
            "run",
            "--pull=never",
            "--name",
            name,
            "--label",
            "orbit.managed=true",
            "--label",
            &format!("orbit.attempt={}", request.attempt_id),
            "--label",
            "orbit.agent=true",
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
            "--network",
            match launch.network {
                AgentNetwork::None => "none",
                AgentNetwork::Host => "host",
            },
            "--cpus",
            &format!("{:.3}", launch.cpu_millis as f64 / 1000.),
            "--memory",
            &format!("{}m", launch.memory_mib),
            "--memory-swap",
            &format!("{}m", launch.memory_mib),
            "--tmpfs",
            "/tmp:rw,nosuid,nodev,size=67108864",
            "--mount",
            &format!("type=bind,src={},dst=/orbit/home", home.display()),
            "--workdir",
            "/orbit/home",
            "--env",
            "HOME=/orbit/home",
            "--env",
            "CODEX_HOME=/orbit/home/.codex",
            "--env",
            "GEMINI_HOME=/orbit/home/.gemini",
            "--env",
            "AGY_ACP_FORCE_FILE_STORAGE=1",
            "--env",
            "XDG_CONFIG_HOME=/orbit/home/.config",
            "--env",
            "XDG_CACHE_HOME=/orbit/home/.cache",
            "--env",
            "NO_BROWSER=1",
            "--env",
            "SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt",
            "--env",
            "REQUESTS_CA_BUNDLE=/etc/ssl/certs/ca-certificates.crt",
            "--env",
            "GIT_CONFIG_NOSYSTEM=1",
            "--env",
            "GIT_CONFIG_GLOBAL=/dev/null",
            "--interactive",
            "--entrypoint",
            &launch.command[0],
            image,
        ])
        .args(&launch.command[1..]);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    Ok(command)
}

/// Resolve the legacy local image-ID form to a Podman repository@digest ref.
/// Podman accepts the form in Orbit's config validation but `podman run` does
/// not accept a bare `sha256:...` image reference.
pub async fn resolve_image(image: &str) -> Result<String> {
    let Some(digest) = image.strip_prefix("sha256:") else {
        return Ok(image.to_string());
    };
    let output = Command::new("podman")
        .args([
            "--remote=false",
            "--cgroup-manager=cgroupfs",
            "image",
            "ls",
            "--no-trunc",
            "--format",
            "{{.Repository}}:{{.Tag}}\t{{.Digest}}",
        ])
        .output()
        .await
        .context("cannot inspect local Podman images")?;
    ensure!(
        output.status.success(),
        "cannot inspect local Podman images: exit={} stderr_present={}",
        output.status.code().unwrap_or(-1),
        !output.stderr.is_empty()
    );
    let expected = format!("sha256:{digest}");
    let mut candidates = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((repository, actual)) = line.split_once('\t') else {
            continue;
        };
        if actual.trim() == expected && repository.trim() != "<none>:<none>" {
            candidates.push(repository.trim().to_string());
        }
    }
    candidates.sort();
    let repository = candidates
        .into_iter()
        .next()
        .with_context(|| format!("pinned local ACP image {image} is not available"))?;
    let resolved = format!("{repository}@{expected}");
    crate::compute::ContainerSpec {
        image: resolved.clone(),
        command: vec!["/bin/true".into()],
    }
    .validate()?;
    Ok(resolved)
}

/// Check the declared Codex launch command before registering a coding worker.
/// This runs only the pinned binary's version command, without auth, mounts,
/// provider network, or a model turn. The configured command remains authoritative.
pub async fn preflight_codex_launch(launch: &Launch) -> Result<()> {
    launch.validate()?;
    ensure!(launch.adapter == Adapter::Codex, "Codex launch required");
    ensure!(
        unsafe { libc::getuid() } != 0,
        "Codex launch requires rootless Podman"
    );
    let image = resolve_image(&launch.image).await?;
    let exists = Command::new("podman")
        .args(["--remote=false", "image", "exists", &image])
        .status()
        .await
        .context("cannot inspect pinned Codex image")?;
    ensure!(
        exists.success(),
        "pinned Codex image is unavailable locally"
    );

    let mut command = Command::new("podman");
    command
        .args([
            "--remote=false",
            "--cgroup-manager=cgroupfs",
            "run",
            "--pull=never",
            "--rm",
            "--network=none",
            "--read-only",
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--pids-limit=32",
            "--init",
            "--log-driver=none",
            "--userns=keep-id",
            "--user",
            &format!("{}:{}", unsafe { libc::getuid() }, unsafe {
                libc::getgid()
            }),
            "--tmpfs",
            "/tmp:rw,nosuid,nodev,size=67108864",
            "--workdir",
            "/tmp",
            "--env",
            "HOME=/tmp",
            "--env",
            "CODEX_HOME=/tmp",
            "--entrypoint",
            &launch.command[0],
            &image,
            "--version",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(20), command.output())
        .await
        .context("Codex launch preflight timed out")?
        .context("Codex launch preflight could not start Podman")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    ensure!(
        output.status.success() && stdout.trim() == format!("codex-cli {}", launch.binary_revision),
        "Codex launch preflight failed: exit={} version_match={} stderr_present={}",
        output.status.code().unwrap_or(-1),
        stdout.trim() == format!("codex-cli {}", launch.binary_revision),
        !stderr.is_empty()
    );
    Ok(())
}

struct RunOutcome {
    code: i32,
    stage: &'static str,
    stderr_present: bool,
    stderr_truncated: bool,
    observation: Option<ProcessObservation>,
}

#[derive(Serialize)]
struct ProcessObservation {
    supervisor_trigger: &'static str,
    child_stdin_opened: bool,
    child_stdin_closed: bool,
    child_stdin_close_cause: &'static str,
    child_stdout_eof_observed: bool,
    child_stderr_eof_observed: bool,
    child_stderr_read_error: bool,
    child_stderr_output_observed: bool,
    child_stderr_truncated: bool,
    child_exit_observed: bool,
    child_exit_code: Option<i32>,
    shutdown_requested: bool,
    session: crate::codex_session::SessionDiagnostics,
}

struct CapturedStderr {
    text: String,
    eof: bool,
    read_error: bool,
    truncated: bool,
}

async fn capture_stderr(stderr: ChildStderr) -> CapturedStderr {
    let mut stderr = stderr;
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 4096];
    let mut truncated = false;
    let eof = loop {
        match stderr.read(&mut buffer).await {
            Ok(0) => break true,
            Ok(size) => {
                if bytes.len() < MAX_LAUNCH_DIAGNOSTIC_BYTES {
                    let keep = (MAX_LAUNCH_DIAGNOSTIC_BYTES - bytes.len()).min(size);
                    bytes.extend_from_slice(&buffer[..keep]);
                    truncated |= keep < size;
                } else {
                    truncated = true;
                }
            }
            Err(_) => break false,
        }
    };
    if truncated {
        bytes.extend_from_slice(b"...[truncated]");
    }
    CapturedStderr {
        text: String::from_utf8_lossy(&bytes).into_owned(),
        eof,
        read_error: !eof,
        truncated,
    }
}

/// Transparent ACP peer proxy. Codex has a separate session translation driver.
pub async fn supervise(path: &Path) -> Result<i32> {
    use tokio::io::AsyncReadExt;
    let bytes = tokio::fs::File::open(path).await?;
    let mut buffer = Vec::new();
    bytes.take(131073).read_to_end(&mut buffer).await?;
    ensure!(buffer.len() <= 131072, "ACP supervisor request too large");
    let request: Request = serde_json::from_slice(&buffer)?;
    uuid::Uuid::parse_str(&request.attempt_id)?;
    ensure!(
        (1..=604800).contains(&request.timeout_seconds),
        "invalid ACP process deadline"
    );
    let root = path
        .parent()
        .context("ACP supervisor request directory missing")?
        .canonicalize()?;
    let home = root.join(format!("acp-home-{}", crate::model::id()));
    let name = format!("orbit-agent-{}-{}", request.attempt_id, crate::model::id());
    let lease = AuthLease::acquire(&request.runtime)?;
    lease.stage(&home, &name, &request.attempt_id)?;
    let run = async {
        let image = resolve_image(&request.runtime.launch.image).await?;
        let codex = request.runtime.launch.adapter == crate::acp_runtime::Adapter::Codex;
        let mut session_diagnostics = crate::codex_session::SessionDiagnostics::default();
        let mut child = command(&request, &home, &name, &image)?
            .spawn()
            .context("ACP agent image launch failed")?;
        let mut input = child.stdin.take().context("ACP input missing")?;
        let mut output = child.stdout.take().context("ACP output missing")?;
        let stderr = child.stderr.take().context("ACP stderr missing")?;
        let stderr_task = tokio::spawn(capture_stderr(stderr));
        let transfer = async {
            if request.runtime.launch.adapter == crate::acp_runtime::Adapter::Codex {
                crate::codex_session::run(
                    &request,
                    crate::acp_wire::Wire::new(output, input, 16 * 1024 * 1024).codex(),
                    crate::acp_wire::Wire::new(
                        tokio::io::stdin(),
                        tokio::io::stdout(),
                        16 * 1024 * 1024,
                    ),
                    &mut session_diagnostics,
                )
                .await
                .map_err(|_| std::io::Error::other("Codex ACP session ended"))?;
                return Ok(());
            }
            let to_agent = async {
                tokio::io::copy(&mut tokio::io::stdin(), &mut input).await?;
                input.shutdown().await
            };
            let to_worker = async {
                tokio::io::copy(&mut output, &mut tokio::io::stdout()).await?;
                Ok::<_, std::io::Error>(())
            };
            tokio::select! { result=to_agent =>result, result=to_worker=>result }
        };
        let (trigger, mut code, mut reaped) = tokio::select! {
            result=transfer=> (
                classify_bridge_result(codex, result.is_ok(), session_diagnostics.outcome),
                125,
                false,
            ),
            status=child.wait()=> ("child_exit", status?.code().unwrap_or(1), true),
            _=tokio::time::sleep(Duration::from_secs(request.timeout_seconds))=> ("supervisor_deadline", 124, false),
        };
        let mut child_exit_code = reaped.then_some(code);
        if !reaped
            && let Ok(Ok(status)) = tokio::time::timeout(Duration::from_secs(2), child.wait()).await
        {
            code = status.code().unwrap_or(1);
            reaped = true;
            child_exit_code = Some(code);
        }
        if !reaped {
            let _ = child.kill().await;
            if let Ok(status) = child.wait().await {
                code = status.code().unwrap_or(1);
                reaped = true;
                child_exit_code = Some(code);
            }
        }
        let captured_stderr = stderr_task.await.unwrap_or(CapturedStderr {
            text: String::new(),
            eof: false,
            read_error: true,
            truncated: false,
        });
        let stage = if codex && trigger == "bridge_error" {
            "app_server_protocol"
        } else if trigger == "peer_eof_after_end_turn" {
            "completed_turn_cleanup"
        } else if trigger == "supervisor_deadline" {
            "supervisor_deadline"
        } else if !captured_stderr.text.trim().is_empty() && code == 125 {
            "container_startup"
        } else if code == 125 {
            "transport"
        } else {
            "container_process"
        };
        let observation = codex.then_some(ProcessObservation {
            supervisor_trigger: trigger,
            child_stdin_opened: true,
            child_stdin_closed: true,
            child_stdin_close_cause: match trigger {
                "child_exit" => "closed_after_child_exit",
                "supervisor_deadline" => "closed_after_supervisor_deadline",
                "bridge_error" => "closed_after_bridge_error",
                "peer_eof_after_end_turn" => "closed_after_completed_turn",
                _ => "closed_after_bridge_return",
            },
            child_stdout_eof_observed: session_diagnostics.app_server_stdout_eof_observed,
            child_stderr_eof_observed: captured_stderr.eof,
            child_stderr_read_error: captured_stderr.read_error,
            child_stderr_output_observed: !captured_stderr.text.is_empty(),
            child_stderr_truncated: captured_stderr.truncated,
            child_exit_observed: reaped,
            child_exit_code,
            shutdown_requested: false,
            session: session_diagnostics,
        });
        Ok::<_, anyhow::Error>(RunOutcome {
            code,
            stage,
            stderr_present: !captured_stderr.text.is_empty(),
            stderr_truncated: captured_stderr.truncated,
            observation,
        })
    }
    .await;
    crate::container::remove("podman", &name).await?;
    lease.finish(&home)?;
    match &run {
        Ok(outcome) => {
            let observation = outcome
                .observation
                .as_ref()
                .map(serde_json::to_value)
                .transpose()?;
            write_cleanup_receipt(
                path,
                &request.attempt_id,
                outcome.code,
                outcome.stage,
                Some(&request.runtime.launch.image),
                DiagnosticStatus {
                    present: outcome.stderr_present,
                    truncated: outcome.stderr_truncated,
                },
                observation,
            )?
        }
        Err(error) => write_cleanup_diagnostic(
            path,
            &request.attempt_id,
            125,
            "container_startup",
            Some(&request.runtime.launch.image),
            Some(&format!("{error:#}")),
        )?,
    }
    Ok(run.map_or(125, |outcome| outcome.code))
}

fn classify_bridge_result(codex: bool, succeeded: bool, session_outcome: &str) -> &'static str {
    if succeeded {
        "bridge_returned"
    } else if codex && session_outcome == "peer_eof_after_end_turn" {
        "peer_eof_after_end_turn"
    } else {
        "bridge_error"
    }
}

#[cfg(test)]
mod tests {
    use super::{ProcessObservation, classify_bridge_result, write_cleanup_receipt};
    use crate::codex_session::SessionDiagnostics;
    use serde_json::json;

    #[test]
    fn expected_peer_eof_after_end_turn_is_not_classified_as_bridge_failure() {
        assert_eq!(
            classify_bridge_result(true, false, "peer_eof_after_end_turn"),
            "peer_eof_after_end_turn"
        );
        assert_eq!(
            classify_bridge_result(true, false, "app_server_error_notification"),
            "bridge_error"
        );
        assert_eq!(
            classify_bridge_result(true, true, "running"),
            "bridge_returned"
        );
    }

    #[test]
    fn codex_lifecycle_receipt_preserves_bounded_structural_evidence() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let request = root.path().join("request.json");
        let session = SessionDiagnostics {
            outcome: "app_server_error_notification",
            turn_outcome: "app_server_error_notification",
            pending_request: Some("turn_start"),
            ..Default::default()
        };
        let observation = ProcessObservation {
            supervisor_trigger: "bridge_error",
            child_stdin_opened: true,
            child_stdin_closed: true,
            child_stdin_close_cause: "closed_after_bridge_error",
            child_stdout_eof_observed: true,
            child_stderr_eof_observed: true,
            child_stderr_read_error: false,
            child_stderr_output_observed: false,
            child_stderr_truncated: false,
            child_exit_observed: true,
            child_exit_code: Some(0),
            shutdown_requested: false,
            session,
        };
        write_cleanup_receipt(
            &request,
            "attempt",
            0,
            "app_server_protocol",
            Some("localhost/orbit-codex@sha256:abc"),
            super::DiagnosticStatus {
                present: false,
                truncated: false,
            },
            Some(serde_json::to_value(observation)?),
        )?;
        let receipt: serde_json::Value =
            serde_json::from_slice(&std::fs::read(request.with_extension("cleanup.json"))?)?;
        assert_eq!(receipt["format"], "orbit-process-cleanup/v4");
        assert_eq!(receipt["observation"]["supervisor_trigger"], "bridge_error");
        assert_eq!(receipt["observation"]["child_exit_code"], 0);
        assert_eq!(receipt["observation"]["child_stdin_closed"], true);
        assert_eq!(
            receipt["observation"]["session"]["outcome"],
            "app_server_error_notification"
        );
        let diagnostic = super::read_cleanup_diagnostic(&request, Some("attempt"))?.unwrap();
        assert!(!diagnostic.contains("secret"));
        assert_eq!(super::read_cleanup(&request, Some("attempt"))?, 0);
        assert!(
            write_cleanup_receipt(
                &root.path().join("oversized.json"),
                "attempt",
                1,
                "protocol",
                None,
                super::DiagnosticStatus {
                    present: false,
                    truncated: false,
                },
                Some(json!({"bounded": "x".repeat(super::MAX_LIFECYCLE_OBSERVATION_BYTES + 1)})),
            )
            .is_err()
        );
        Ok(())
    }
}
