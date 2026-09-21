//! Independent OCI agent supervisor with a worker stdio lifeline and auth quarantine.
use crate::{
    acp_files::Root,
    acp_runtime::{AgentNetwork, Runtime},
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
    use std::io::Write;
    let diagnostic = diagnostic.map(|value| sanitize_diagnostic(value, request));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(request.with_extension("cleanup.json"))?;
    file.write_all(&serde_json::to_vec(&serde_json::json!({
        "format":"orbit-process-cleanup/v2",
        "request":request.file_name().and_then(|s|s.to_str()).context("invalid cleanup request filename")?,
        "attempt_id":attempt,
        "exit_code":code,
        "runtime":"podman",
        "launch_stage":launch_stage,
        "image":image,
        "diagnostic":diagnostic
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
        4096,
    )?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    ensure!(
        (value["format"] == "orbit-process-cleanup/v1"
            || value["format"] == "orbit-process-cleanup/v2")
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
        65536,
    )?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    ensure!(
        (value["format"] == "orbit-process-cleanup/v1"
            || value["format"] == "orbit-process-cleanup/v2")
            && value["request"] == request.file_name().unwrap().to_str().unwrap()
            && attempt.is_none_or(|id| value["attempt_id"] == id),
        "foreign cleanup receipt"
    );
    if value.get("launch_stage").is_none()
        && value.get("runtime").is_none()
        && value.get("image").is_none()
        && value.get("diagnostic").is_none()
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
    if let Some(diagnostic) = value["diagnostic"].as_str() {
        message.push_str(" diagnostic=");
        message.push_str(diagnostic);
    }
    Ok(Some(message))
}

fn sanitize_diagnostic(value: &str, request: &Path) -> String {
    let mut value = value.replace('\r', "");
    if let Ok(bytes) = std::fs::read(request)
        && let Ok(request_value) = serde_json::from_slice::<Request>(&bytes)
    {
        let auth_path = request_value
            .runtime
            .auth
            .path
            .to_string_lossy()
            .into_owned();
        value = value.replace(&auth_path, "<credential-path>");
    }
    if let Some(path) = request.parent() {
        let control_path = path.to_string_lossy().into_owned();
        value = value.replace(&control_path, "<control-path>");
    }
    value = value
        .chars()
        .map(|ch| {
            if ch == '\n' || ch == '\t' || !ch.is_control() {
                ch
            } else {
                '?'
            }
        })
        .collect();
    if value.len() > MAX_LAUNCH_DIAGNOSTIC_BYTES {
        value.truncate(MAX_LAUNCH_DIAGNOSTIC_BYTES);
        value.push_str("...[truncated]");
    }
    value
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
        "cannot inspect local Podman images: {}",
        sanitize_diagnostic(&String::from_utf8_lossy(&output.stderr), Path::new(""))
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

struct RunOutcome {
    code: i32,
    stage: &'static str,
    stderr: String,
}

async fn capture_stderr(stderr: ChildStderr) -> String {
    let mut stderr = stderr;
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 4096];
    let mut truncated = false;
    while let Ok(size) = stderr.read(&mut buffer).await {
        if size == 0 {
            break;
        }
        if bytes.len() < MAX_LAUNCH_DIAGNOSTIC_BYTES {
            let keep = (MAX_LAUNCH_DIAGNOSTIC_BYTES - bytes.len()).min(size);
            bytes.extend_from_slice(&buffer[..keep]);
            truncated |= keep < size;
        } else {
            truncated = true;
        }
    }
    if truncated {
        bytes.extend_from_slice(b"...[truncated]");
    }
    String::from_utf8_lossy(&bytes).into_owned()
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
        let (mut code, mut reaped) = tokio::select! {
            _=transfer=> (125, false),
            status=child.wait()=> (status?.code().unwrap_or(1), true),
            _=tokio::time::sleep(Duration::from_secs(request.timeout_seconds))=> (124, false),
        };
        if !reaped
            && let Ok(Ok(status)) = tokio::time::timeout(Duration::from_secs(2), child.wait()).await
        {
            code = status.code().unwrap_or(1);
            reaped = true;
        }
        if !reaped {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        let stderr = stderr_task.await.unwrap_or_default();
        let stage = if !stderr.trim().is_empty() && code == 125 {
            "container_startup"
        } else if code == 125 {
            "transport"
        } else {
            "container_process"
        };
        Ok::<_, anyhow::Error>(RunOutcome {
            code,
            stage,
            stderr,
        })
    }
    .await;
    crate::container::remove("podman", &name).await?;
    lease.finish(&home)?;
    match &run {
        Ok(outcome) => write_cleanup_diagnostic(
            path,
            &request.attempt_id,
            outcome.code,
            outcome.stage,
            Some(&request.runtime.launch.image),
            (!outcome.stderr.is_empty()).then_some(outcome.stderr.as_str()),
        )?,
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

#[cfg(test)]
mod tests {
    use super::sanitize_diagnostic;

    #[test]
    fn launch_diagnostic_removes_control_data() {
        let request = std::path::Path::new("/tmp/orbit-control/request.json");
        let value = sanitize_diagnostic(
            "/home/user/.orbit/credentials/secret\n\u{1b}[31mimage not known\u{1b}[0m",
            request,
        );
        assert!(!value.contains('\u{1b}'));
        assert!(value.contains("image not known"));
    }
}
