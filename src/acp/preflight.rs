//! ACP installation preflight. This does not authorize workflow execution.
//!
//! A successful initialize response proves wire compatibility, not broker mediation,
//! authentication readiness, or safe native tools. No session or prompt is sent.
use agent_client_protocol::{self as protocol, Agent as _};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    task::{Context as TaskContext, Poll},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

const MAX_FRAME_BYTES: usize = 1024 * 1024;
const MAX_WIRE_BYTES: usize = 4 * MAX_FRAME_BYTES;
const MAX_MESSAGES: usize = 128;

/// Private operator configuration, never a Definition or a worker assignment.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeConfig {
    pub command: PathBuf,
    pub args: Vec<String>,
    /// Canonical regular file paths and their SHA-256 digests, including command.
    /// Operators also pin scripts and the underlying agent installation here.
    pub files: BTreeMap<PathBuf, String>,
    pub expected_agent_name: String,
    pub expected_agent_version: String,
    pub timeout_seconds: u64,
}

fn safe_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@/._+-".contains(&b))
}

impl ProbeConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.command.is_absolute()
                && self.files.contains_key(&self.command)
                && (1..=64).contains(&self.files.len()),
            "pin an absolute executable and at most 64 installation files"
        );
        ensure!(
            self.args.len() <= 64
                && self
                    .args
                    .iter()
                    .all(|s| s.len() <= 4096 && !s.contains('\0'))
                && self.args.iter().map(String::len).sum::<usize>() <= 16384,
            "ACP launch arguments exceed bounds"
        );
        ensure!(
            safe_label(&self.expected_agent_name)
                && safe_label(&self.expected_agent_version)
                && (1..=30).contains(&self.timeout_seconds),
            "invalid expected ACP identity or probe timeout"
        );
        for (path, hash) in &self.files {
            ensure!(
                path.is_absolute()
                    && hash.len() == 64
                    && hash
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "invalid installation file pin"
            );
        }
        Ok(())
    }

    async fn verify(&self) -> Result<()> {
        self.validate()?;
        for (path, expected) in &self.files {
            ensure!(
                tokio::fs::canonicalize(path).await? == *path,
                "installation paths must be canonical, without symlink components"
            );
            let mut options = tokio::fs::OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
            let mut file = options.open(path).await?;
            let metadata = file.metadata().await?;
            ensure!(
                metadata.is_file(),
                "installation pin must be a regular file"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                ensure!(
                    metadata.permissions().mode() & 0o022 == 0,
                    "installation file is writable by group or others"
                );
                if path == &self.command {
                    ensure!(
                        metadata.permissions().mode() & 0o111 != 0,
                        "ACP command is not executable"
                    );
                }
            }
            ensure!(
                metadata.len() <= 512 * 1024 * 1024,
                "installation file exceeds 512 MiB"
            );
            let mut hash = Sha256::new();
            let mut buffer = [0u8; 65536];
            let mut read = 0u64;
            loop {
                let size = file.read(&mut buffer).await?;
                if size == 0 {
                    break;
                }
                read += size as u64;
                ensure!(
                    read <= metadata.len(),
                    "installation changed during verification"
                );
                hash.update(&buffer[..size]);
            }
            ensure!(
                read == metadata.len() && hex::encode(hash.finalize()) == *expected,
                "ACP installation checksum mismatch"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct ProbeReport {
    pub format: &'static str,
    pub config_digest: String,
    pub protocol_version: u16,
    pub agent_name: String,
    pub agent_version: String,
    pub auth_method_ids: Vec<String>,
    pub supports_load_session: bool,
    pub supports_mcp_http: bool,
    pub workflow_execution_supported: bool,
    pub broker_mediation: &'static str,
    pub authentication: &'static str,
    pub direct_child_reaped: bool,
}

struct ProbeClient;

#[async_trait::async_trait(?Send)]
impl protocol::Client for ProbeClient {
    async fn request_permission(
        &self,
        _: protocol::RequestPermissionRequest,
    ) -> protocol::Result<protocol::RequestPermissionResponse> {
        Ok(protocol::RequestPermissionResponse::new(
            protocol::RequestPermissionOutcome::Cancelled,
        ))
    }

    async fn session_notification(&self, _: protocol::SessionNotification) -> protocol::Result<()> {
        // No content from the peer, including reasoning, is retained or logged.
        Ok(())
    }

    async fn ext_method(&self, _: protocol::ExtRequest) -> protocol::Result<protocol::ExtResponse> {
        Err(protocol::Error::method_not_found())
    }
}

/// Bound frames before the SDK's read_line allocates them and bound its total
/// incoming queue. Applies to all traffic, including ignored extension messages.
struct BoundedRead<R> {
    inner: R,
    frame: usize,
    total: usize,
    messages: usize,
}

impl<R> BoundedRead<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            frame: 0,
            total: 0,
            messages: 0,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for BoundedRead<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = output.filled().len();
        match Pin::new(&mut self.inner).poll_read(cx, output) {
            Poll::Ready(Ok(())) => {
                for &byte in &output.filled()[before..] {
                    self.total += 1;
                    self.frame += 1;
                    if byte == b'\n' {
                        self.messages += 1;
                        self.frame = 0;
                    }
                    if self.frame > MAX_FRAME_BYTES
                        || self.total > MAX_WIRE_BYTES
                        || self.messages > MAX_MESSAGES
                    {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "ACP input exceeds probe bounds",
                        )));
                    }
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

struct ProcessGroup(u32);
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.0 as i32), libc::SIGKILL);
        }
    }
}

/// Run only initialize in a fresh private directory. The caller owns retention.
/// Use an operator-controlled installation: hashing is not an OS sandbox.
pub async fn probe(config: &ProbeConfig, workspaces: &Path) -> Result<ProbeReport> {
    ensure!(
        cfg!(target_os = "linux"),
        "ACP preflight currently requires Linux"
    );
    tokio::time::timeout(Duration::from_secs(30), config.verify())
        .await
        .context("ACP installation verification timed out")??;
    let root = tokio::fs::canonicalize(workspaces)
        .await
        .context("probe workspace parent must exist")?;
    let directory = root.join(format!("acp-probe-{}", crate::model::id()));
    let mut builder = tokio::fs::DirBuilder::new();
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(&directory).await?;
    // Do not reuse the user's credential/config home. The probe never authenticates,
    // creates a session, or sends a model prompt.
    let mut command = tokio::process::Command::new(&config.command);
    command
        .args(&config.args)
        .current_dir(&directory)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &directory)
        .env("XDG_CONFIG_HOME", &directory)
        .env("XDG_CACHE_HOME", &directory)
        .env("NO_BROWSER", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .context("cannot spawn pinned ACP installation")?;
    let group = ProcessGroup(child.id().context("ACP process has no ID")?);
    let input = BoundedRead::new(child.stdout.take().context("ACP stdout missing")?).compat();
    let output = child
        .stdin
        .take()
        .context("ACP stdin missing")?
        .compat_write();
    let result = tokio::task::LocalSet::new().run_until(async {
        let (connection, io) = protocol::ClientSideConnection::new(ProbeClient, output, input, |task| { tokio::task::spawn_local(task); });
        let request = connection.initialize(protocol::InitializeRequest::new(protocol::ProtocolVersion::V1)
            .client_info(protocol::Implementation::new("orbit", env!("CARGO_PKG_VERSION"))));
        tokio::pin!(request, io);
        tokio::select! {
            response = &mut request => response.map_err(|_| anyhow::anyhow!("ACP initialization rejected or connection closed")),
            _ = &mut io => anyhow::bail!("ACP transport closed or rejected malformed/oversized input"),
            _ = tokio::time::sleep(Duration::from_secs(config.timeout_seconds)) => anyhow::bail!("ACP initialization timed out"),
        }
    }).await;
    // Kill the process group as well as the direct child, including on failure.
    // A reaped direct child is not proof that a malicious child could not escape.
    drop(group);
    child.start_kill().ok();
    tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await
        .context("ACP direct child cleanup unconfirmed")??;
    let response = result?;
    ensure!(
        response.protocol_version == protocol::ProtocolVersion::V1,
        "ACP negotiated an unsupported protocol version"
    );
    let info = response
        .agent_info
        .context("ACP agent must report its pinned identity")?;
    ensure!(
        info.name == config.expected_agent_name && info.version == config.expected_agent_version,
        "ACP agent identity/version mismatch"
    );
    ensure!(
        response.auth_methods.len() <= 32,
        "too many ACP authentication methods"
    );
    let mut auth_method_ids = Vec::new();
    for method in response.auth_methods {
        let id = method.id().to_string();
        ensure!(safe_label(&id), "invalid ACP authentication method ID");
        auth_method_ids.push(id);
    }
    Ok(ProbeReport {
        format: "orbit-acp-probe/v1",
        config_digest: crate::model::digest(&serde_json::to_vec(config)?),
        protocol_version: 1,
        agent_name: info.name,
        agent_version: info.version,
        auth_method_ids,
        supports_load_session: response.agent_capabilities.load_session,
        supports_mcp_http: response.agent_capabilities.mcp_capabilities.http,
        workflow_execution_supported: false,
        broker_mediation: "not_verified",
        authentication: "not_tested",
        direct_child_reaped: true,
    })
}
