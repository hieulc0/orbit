//! Operator-only provider enrollment. This is deliberately separate from
//! worker launches and never creates an ACP session, broker, or model turn.
use crate::{
    acp_wire::Wire,
    credential_registry::{Credential, CredentialStatus, CredentialStore, RepresentationState},
    secret_backend::{LOCAL_PRIVATE_ID, LocalPrivateSecretBackend, SecretBackend, SecretBytes},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tempfile::TempDir;
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
    sync::mpsc,
};
use zeroize::{Zeroize, Zeroizing};

pub const ANTIGRAVITY_IMAGE: &str = "localhost/orbit-antigravity-runtime:agy_acp_server_1.1.1-orbit-terminal-v2@sha256:3e7415f6f732ae4168b98a6fb0e14e0fba965020cf5cc1fc5a3b35867b4cf830";
pub const ANTIGRAVITY_DIGEST: &str =
    "sha256:3e7415f6f732ae4168b98a6fb0e14e0fba965020cf5cc1fc5a3b35867b4cf830";
const ACP_EXECUTABLE: &str = "/opt/antigravity/agy_acp_server.par";
const PERSONAL: &str = "oauth-personal";
const TOKEN: &str = "acp_token.json";
const SETTINGS: &str = "settings.json";
const BUNDLE_MAGIC: &[u8; 8] = b"ORACP1\0\0";
const MAX_ARTIFACT: usize = 512 * 1024;

/// This profile is constructible only here, from an explicit local operator
/// credential command. Normal agent Launch does not accept it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnrollmentNetwork {
    HostLoopback,
}

#[derive(Debug)]
struct EnrollmentProfile {
    network: EnrollmentNetwork,
    image: &'static str,
    no_repository: bool,
    no_broker: bool,
    no_agent_tools: bool,
}

impl EnrollmentProfile {
    fn operator_antigravity() -> Self {
        Self {
            network: EnrollmentNetwork::HostLoopback,
            image: ANTIGRAVITY_IMAGE,
            no_repository: true,
            no_broker: true,
            no_agent_tools: true,
        }
    }
}

/// Minimal provider-neutral identity for an operator enrollment adapter.
/// Discovery, interaction, capture and validation belong to the concrete
/// adapter; a PAT, SSH, or external-backend adapter need not implement ACP or
/// a HOME-based artifact exchange.
pub trait CredentialEnrollmentProvider {
    fn provider_id(&self) -> &'static str;
    fn interface_id(&self) -> &'static str;
    fn selected_auth_method(&self) -> &'static str;
}

pub struct AntigravityAcpEnrollmentProvider;

impl CredentialEnrollmentProvider for AntigravityAcpEnrollmentProvider {
    fn provider_id(&self) -> &'static str {
        "antigravity"
    }
    fn interface_id(&self) -> &'static str {
        "acp"
    }
    fn selected_auth_method(&self) -> &'static str {
        PERSONAL
    }
}

impl AntigravityAcpEnrollmentProvider {
    fn require_advertised(&self, initialize: &Value) -> Result<()> {
        ensure!(
            initialize["protocolVersion"] == 1,
            "ACP protocol version mismatch"
        );
        ensure!(
            initialize["agentInfo"]["name"] == "antigravity-acp"
                && initialize["agentInfo"]["version"] == "agy_acp_server_1.1.1",
            "ACP runtime identity mismatch"
        );
        let methods = initialize["authMethods"]
            .as_array()
            .context("ACP auth discovery missing")?;
        ensure!(methods.len() <= 32, "ACP auth discovery too large");
        let method = methods
            .iter()
            .find(|v| v["id"] == PERSONAL)
            .context("oauth-personal is not advertised")?;
        // ACP's omitted type is the protocol-default agent-managed method.
        ensure!(
            method.get("type").is_none() || method["type"] == "agent",
            "oauth-personal is not agent-managed"
        );
        Ok(())
    }

    fn capture(&self, home: &Path) -> Result<SecretBytes> {
        let directory = auth_directory(home, false)?;
        let token = private_read(&directory.join(TOKEN), MAX_ARTIFACT)?;
        let settings = private_read(&directory.join(SETTINGS), MAX_ARTIFACT)?;
        let mut bytes = Vec::with_capacity(
            BUNDLE_MAGIC.len() + 8 + token.expose().len() + settings.expose().len(),
        );
        bytes.extend_from_slice(BUNDLE_MAGIC);
        for item in [&token, &settings] {
            bytes.extend_from_slice(&(item.expose().len() as u32).to_be_bytes());
            bytes.extend_from_slice(item.expose());
        }
        SecretBytes::new(bytes)
    }

    fn stage(&self, home: &Path, bundle: &SecretBytes) -> Result<()> {
        let (token, settings) = decode_bundle(bundle)?;
        let directory = auth_directory(home, true)?;
        private_write(&directory.join(TOKEN), token.expose())?;
        private_write(&directory.join(SETTINGS), settings.expose())?;
        Ok(())
    }
}

fn checked_directory(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path).context("private enrollment directory unavailable")?;
    ensure!(
        meta.is_dir()
            && !meta.file_type().is_symlink()
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.mode() & 0o022 == 0,
        "private enrollment directory unsafe"
    );
    Ok(())
}

fn auth_directory(home: &Path, create: bool) -> Result<PathBuf> {
    checked_directory(home)?;
    let gemini = home.join(".gemini");
    let acp = gemini.join("antigravity-acp");
    if create {
        for path in [&gemini, &acp] {
            if !path.exists() {
                fs::DirBuilder::new().mode(0o700).create(path)?;
            }
        }
    }
    checked_directory(&gemini)?;
    checked_directory(&acp)?;
    Ok(acp)
}

fn private_read(path: &Path, max: usize) -> Result<SecretBytes> {
    let meta = fs::symlink_metadata(path).context("ACP credential artifact missing")?;
    ensure!(
        meta.is_file()
            && !meta.file_type().is_symlink()
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.nlink() == 1
            && meta.mode() & 0o022 == 0
            && meta.len() > 0
            && meta.len() <= max as u64,
        "ACP credential artifact unsafe"
    );
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(max as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= max, "ACP credential artifact exceeds bound");
    SecretBytes::new(bytes)
}

fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_ARTIFACT,
        "ACP artifact size invalid"
    );
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    File::open(path.parent().context("ACP artifact parent missing")?)?.sync_all()?;
    Ok(())
}

fn decode_bundle(bundle: &SecretBytes) -> Result<(SecretBytes, SecretBytes)> {
    let bytes = bundle.expose();
    ensure!(
        bytes.starts_with(BUNDLE_MAGIC),
        "invalid ACP credential bundle"
    );
    let mut cursor = BUNDLE_MAGIC.len();
    let mut parts = Vec::with_capacity(2);
    for _ in 0..2 {
        ensure!(cursor + 4 <= bytes.len(), "invalid ACP credential bundle");
        let size = u32::from_be_bytes(bytes[cursor..cursor + 4].try_into()?) as usize;
        cursor += 4;
        ensure!(
            size > 0 && size <= MAX_ARTIFACT && cursor + size <= bytes.len(),
            "invalid ACP credential bundle"
        );
        parts.push(SecretBytes::new(bytes[cursor..cursor + size].to_vec())?);
        cursor += size;
    }
    ensure!(cursor == bytes.len(), "invalid ACP credential bundle");
    let settings = parts.pop().context("missing ACP settings")?;
    let token = parts.pop().context("missing ACP token")?;
    Ok((token, settings))
}

fn new_private_home() -> Result<(TempDir, PathBuf)> {
    let root = tempfile::Builder::new()
        .prefix("orbit-enroll-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let home = root.path().join("home");
    fs::DirBuilder::new().mode(0o700).create(&home)?;
    checked_directory(root.path())?;
    checked_directory(&home)?;
    let root_mode = fs::metadata(root.path())?.mode() & 0o7777;
    let home_mode = fs::metadata(&home)?.mode() & 0o7777;
    ensure!(
        root_mode == 0o700 && home_mode == 0o700,
        "enrollment root or HOME not owner-only: root={root_mode:o} home={home_mode:o}"
    );
    Ok((root, home))
}

async fn verify_local_image() -> Result<()> {
    ensure!(
        unsafe { libc::geteuid() } != 0,
        "enrollment requires rootless Podman"
    );
    let rootless = Command::new("podman")
        .args([
            "--remote=false",
            "--cgroup-manager=cgroupfs",
            "info",
            "--format",
            "{{.Host.Security.Rootless}}",
        ])
        .output()
        .await
        .context("rootless Podman check failed")?;
    ensure!(
        rootless.status.success() && rootless.stdout == b"true\n",
        "enrollment requires the rootless Podman store"
    );
    let output = Command::new("podman")
        .args([
            "--remote=false",
            "--cgroup-manager=cgroupfs",
            "image",
            "inspect",
            "--format",
            "{{json .RepoDigests}}",
            ANTIGRAVITY_IMAGE,
        ])
        .output()
        .await
        .context("pinned Antigravity image inspection failed")?;
    ensure!(
        output.status.success() && output.stdout.len() <= 8192,
        "pinned Antigravity image unavailable"
    );
    let digests: Vec<String> = serde_json::from_slice(&output.stdout)
        .map_err(|_| anyhow::anyhow!("pinned image metadata invalid"))?;
    ensure!(
        digests.iter().any(|d| d.ends_with(ANTIGRAVITY_DIGEST)),
        "pinned Antigravity image digest mismatch"
    );
    Ok(())
}

enum AuthNotice {
    Url(Zeroizing<String>),
}

fn parse_auth_url(line: &str) -> Option<String> {
    let pos = line.find("https://accounts.google.com/")?;
    let candidate = line[pos..].split_whitespace().next()?;
    if candidate.len() > 8192 {
        return None;
    }
    let parsed = reqwest::Url::parse(candidate).ok()?;
    if parsed.scheme() != "https" || parsed.host_str() != Some("accounts.google.com") {
        return None;
    }
    let redirect = parsed
        .query_pairs()
        .find(|(key, _)| key == "redirect_uri")?
        .1;
    let callback = reqwest::Url::parse(&redirect).ok()?;
    if callback.scheme() != "http"
        || callback.host_str() != Some("127.0.0.1")
        || callback.port().is_none()
        || callback.username() != ""
        || callback.password().is_some()
    {
        return None;
    }
    Some(candidate.to_string())
}

async fn read_auth_stderr(mut stderr: tokio::process::ChildStderr, tx: mpsc::Sender<AuthNotice>) {
    let mut chunk = [0u8; 1024];
    let mut line = Vec::new();
    let mut total = 0usize;
    while let Ok(n) = stderr.read(&mut chunk).await {
        if n == 0 {
            break;
        }
        total = total.saturating_add(n);
        if total > 65536 {
            continue;
        } // Keep draining; never block the provider on stderr.
        for byte in &chunk[..n] {
            if *byte == b'\n' {
                if let Ok(text) = std::str::from_utf8(&line)
                    && let Some(url) = parse_auth_url(text)
                {
                    let _ = tx.send(AuthNotice::Url(Zeroizing::new(url))).await;
                }
                line.zeroize();
            } else if line.len() < 8192 {
                line.push(*byte);
            }
        }
    }
    line.zeroize();
    chunk.zeroize();
}

struct AcpEnrollmentSession {
    child: Child,
    wire: Wire,
    notices: mpsc::Receiver<AuthNotice>,
    reader: tokio::task::JoinHandle<()>,
    container_name: String,
    _root: TempDir,
    home: PathBuf,
}

impl AcpEnrollmentSession {
    async fn launch(
        staged: Option<&SecretBytes>,
        provider: &AntigravityAcpEnrollmentProvider,
    ) -> Result<Self> {
        let profile = EnrollmentProfile::operator_antigravity();
        ensure!(
            profile.no_repository && profile.no_broker && profile.no_agent_tools,
            "unsafe enrollment profile"
        );
        let (root, home) = new_private_home()?;
        if let Some(bundle) = staged {
            provider.stage(&home, bundle)?;
        }
        let name = format!("orbit-credential-enroll-{}", uuid::Uuid::new_v4());
        let mut command = Command::new("podman");
        command.args([
            "--remote=false",
            "--cgroup-manager=cgroupfs",
            "run",
            "--rm",
            "--pull=never",
            "--name",
            &name,
            "--read-only",
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--pids-limit=128",
            "--log-driver=none",
            "--userns=keep-id",
            "--user",
            &format!("{}:{}", unsafe { libc::getuid() }, unsafe {
                libc::getgid()
            }),
            "--network",
            match profile.network {
                EnrollmentNetwork::HostLoopback => "host",
            },
            "--cpus=1",
            "--memory=512m",
            "--memory-swap=512m",
            "--tmpfs",
            "/tmp:rw,nosuid,nodev,size=67108864",
            "--mount",
            &format!("type=bind,src={},dst=/orbit/home", home.display()),
            "--workdir",
            "/orbit/home",
            "--env",
            "HOME=/orbit/home",
            "--env",
            "GEMINI_HOME=/orbit/home/.gemini",
            "--env",
            "AGY_ACP_FORCE_FILE_STORAGE=1",
            "--env",
            "XDG_CONFIG_HOME=/orbit/home/.config",
            "--env",
            "XDG_CACHE_HOME=/orbit/home/.cache",
            "--env",
            "BROWSER=/orbit/no-browser",
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
            ACP_EXECUTABLE,
            profile.image,
        ]);
        // Do not clear HOME/XDG_RUNTIME_DIR on the *host* Podman client: that
        // would silently select a different rootless store. Container env is
        // solely the explicit --env list above.
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .context("cannot start pinned Antigravity enrollment runtime")?;
        let stdout = child.stdout.take().context("ACP stdout unavailable")?;
        let stdin = child.stdin.take().context("ACP stdin unavailable")?;
        let stderr = child.stderr.take().context("ACP stderr unavailable")?;
        let (tx, notices) = mpsc::channel(2);
        let reader = tokio::spawn(read_auth_stderr(stderr, tx));
        Ok(Self {
            child,
            wire: Wire::new(stdout, stdin, 4 * 1024 * 1024),
            notices,
            reader,
            container_name: name,
            _root: root,
            home,
        })
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.wire.request(method, params).await?;
        let response = tokio::time::timeout(Duration::from_secs(30), self.wire.read())
            .await
            .context("ACP response timed out")??;
        ensure!(
            response.get("method").is_none(),
            "unexpected ACP request during enrollment"
        );
        Wire::result(response, &id)
    }

    async fn initialize(&mut self, provider: &AntigravityAcpEnrollmentProvider) -> Result<()> {
        let response = self.request("initialize", json!({"protocolVersion":1,"clientCapabilities":{},
            "clientInfo":{"name":"orbit-credential-enrollment","version":env!("CARGO_PKG_VERSION")}})).await?;
        provider.require_advertised(&response)
    }

    async fn authenticate(&mut self, interactive: bool) -> Result<()> {
        let authentication = tokio::time::timeout(
            Duration::from_secs(if interactive { 310 } else { 45 }),
            authenticate_protocol(&mut self.wire, &mut self.notices, interactive, |url| {
                // Deliberately the only sink for the ephemeral URL.
                eprintln!(
                    "Open this URL in your browser:\n{url}\nWaiting for Google authentication..."
                );
            }),
        );
        tokio::select! {
            result = authentication => result.context("ACP authentication timed out")?,
            result = tokio::signal::ctrl_c() => {
                result.context("operator cancellation signal unavailable")?;
                anyhow::bail!("operator cancelled ACP authentication");
            }
        }
    }

    async fn stop(mut self) -> Result<(TempDir, PathBuf)> {
        self.child.start_kill().ok();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
        self.reader.abort();
        let output = tokio::time::timeout(
            Duration::from_secs(15),
            Command::new("podman")
                .args([
                    "--remote=false",
                    "--cgroup-manager=cgroupfs",
                    "rm",
                    "--force",
                    "--ignore",
                    &self.container_name,
                ])
                .output(),
        )
        .await
        .context("enrollment container cleanup timed out")?
        .context("enrollment container cleanup failed")?;
        ensure!(
            output.status.success(),
            "enrollment container cleanup unconfirmed"
        );
        Ok((self._root, self.home))
    }
}

async fn authenticate_protocol(
    wire: &mut Wire,
    notices: &mut mpsc::Receiver<AuthNotice>,
    interactive: bool,
    mut show_url: impl FnMut(&str),
) -> Result<()> {
    let id = wire
        .request("authenticate", json!({"methodId":PERSONAL}))
        .await?;
    {
        let mut url_seen = false;
        let mut stderr_open = true;
        loop {
            tokio::select! {
                notice = notices.recv(), if stderr_open => {
                    if let Some(AuthNotice::Url(url)) = notice {
                        ensure!(interactive && !url_seen, "reuse requested interactive Google login");
                        url_seen = true;
                        show_url(&url);
                    } else {
                        stderr_open = false;
                    }
                }
                incoming = wire.read() => {
                    let response = incoming?;
                    ensure!(response.get("method").is_none(), "unexpected ACP request during authentication");
                    let result = Wire::result(response, &id)?;
                    ensure!(result.is_object(), "malformed ACP authenticate result");
                    if interactive { ensure!(url_seen, "authentication completed without operator interaction"); }
                    return Ok(());
                }
            }
        }
    }
}

pub async fn enroll_antigravity_personal(pool: &PgPool, reference: &str) -> Result<Credential> {
    ensure!(
        crate::credential_registry::valid_reference(reference),
        "invalid credential reference"
    );
    let provider = AntigravityAcpEnrollmentProvider;
    verify_local_image().await?;
    let backend = LocalPrivateSecretBackend::default_for_operator()?;
    let store = CredentialStore::new(pool);
    store
        .create(
            provider.provider_id(),
            reference,
            None,
            provider.selected_auth_method(),
            LOCAL_PRIVATE_ID,
        )
        .await?;
    let pending = store
        .prepare_representation(
            reference,
            provider.interface_id(),
            &[],
            backend.backend_id(),
        )
        .await?;
    let mut session = AcpEnrollmentSession::launch(None, &provider).await?;
    let enrollment = async {
        session.initialize(&provider).await?;
        session.authenticate(true).await
    }
    .await;
    let (enrollment_root, enrollment_home) = session.stop().await?;
    enrollment?;
    let bundle = provider.capture(&enrollment_home)?;
    enrollment_root
        .close()
        .context("enrollment HOME cleanup failed")?;
    let locator = pending
        .secret_locator
        .context("pending ACP locator missing")?;
    backend.create(locator, bundle).await?;
    let staged = backend.read(locator).await?;
    let mut reuse = AcpEnrollmentSession::launch(Some(&staged), &provider).await?;
    let validation = async {
        reuse.initialize(&provider).await?;
        reuse.authenticate(false).await
    }
    .await;
    let (validation_root, validation_home) = reuse.stop().await?;
    validation?;
    // A refresh during the non-model reuse check can rotate provider material.
    // Preserve the validated state before making the catalog active.
    let refreshed = provider.capture(&validation_home)?;
    backend.replace(locator, refreshed).await?;
    validation_root
        .close()
        .context("reuse HOME cleanup failed")?;
    let finalized = store
        .finalize_validated_representation(&backend, &pending.id)
        .await?;
    ensure!(
        finalized.state == RepresentationState::Stored
            && store
                .get(reference)
                .await?
                .is_some_and(|c| c.status == CredentialStatus::Enrolled),
        "ACP credential finalization incomplete"
    );
    store
        .get(reference)
        .await?
        .context("enrolled credential missing")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_selection_requires_advertisement_and_pinned_identity() {
        let provider = AntigravityAcpEnrollmentProvider;
        let initialized = json!({"protocolVersion":1,"agentInfo":{"name":"antigravity-acp","version":"agy_acp_server_1.1.1"},
            "authMethods":[{"id":"oauth-personal","name":"Log in with Google"}]});
        assert!(provider.require_advertised(&initialized).is_ok());
        let mut missing = initialized.clone();
        missing["authMethods"] = json!([{"id":"oauth-business"}]);
        assert!(provider.require_advertised(&missing).is_err());
        let mut terminal = initialized;
        terminal["authMethods"][0]["type"] = json!("terminal");
        assert!(provider.require_advertised(&terminal).is_err());
    }

    #[test]
    fn bundle_stages_only_whitelisted_files_and_redacts() -> Result<()> {
        let provider = AntigravityAcpEnrollmentProvider;
        let (_root, home) = new_private_home()?;
        let directory = auth_directory(&home, true)?;
        private_write(&directory.join(TOKEN), b"token-marker")?;
        private_write(&directory.join(SETTINGS), b"settings-marker")?;
        private_write(&directory.join("conversations.json"), b"not-copied")?;
        let bundle = provider.capture(&home)?;
        assert!(!format!("{bundle:?}").contains("token-marker"));
        let (_other_root, other_home) = new_private_home()?;
        provider.stage(&other_home, &bundle)?;
        let staged = auth_directory(&other_home, false)?;
        assert!(staged.join(TOKEN).exists() && staged.join(SETTINGS).exists());
        assert!(!staged.join("conversations.json").exists());
        assert_eq!(
            fs::metadata(staged.join(TOKEN))?.permissions().mode() & 0o777,
            0o600
        );
        Ok(())
    }

    #[test]
    fn rejects_symlink_and_bad_bundle() -> Result<()> {
        let provider = AntigravityAcpEnrollmentProvider;
        let (_root, home) = new_private_home()?;
        let directory = auth_directory(&home, true)?;
        std::os::unix::fs::symlink("/tmp/no-token", directory.join(TOKEN))?;
        private_write(&directory.join(SETTINGS), b"settings")?;
        assert!(provider.capture(&home).is_err());
        assert!(
            provider
                .stage(&home, &SecretBytes::new(b"wrong bundle".to_vec())?)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn url_is_bounded_and_google_only() {
        assert!(
            parse_auth_url("Open: https://accounts.google.com/o/oauth2/v2/auth?redirect_uri=http%3A%2F%2F127.0.0.1%3A36123%2F&state=x").is_some()
        );
        assert!(parse_auth_url("https://accounts.google.com/o/oauth2/v2/auth?redirect_uri=https%3A%2F%2Fevil.example%2F").is_none());
        assert!(parse_auth_url("https://evil.example/path").is_none());
        assert!(parse_auth_url("https://accounts.google.com.evil.example/").is_none());
    }

    #[test]
    fn host_loopback_profile_is_operator_only_and_has_no_workflow_effects() {
        let profile = EnrollmentProfile::operator_antigravity();
        assert_eq!(profile.network, EnrollmentNetwork::HostLoopback);
        assert_eq!(profile.image, ANTIGRAVITY_IMAGE);
        assert!(profile.no_repository && profile.no_broker && profile.no_agent_tools);
        // The private constructor and launcher live outside acp_runtime::Launch;
        // worker assignments cannot request this profile.
    }

    #[tokio::test]
    async fn mocked_acp_auth_requires_interaction_and_reuse_rejects_new_login() -> Result<()> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        async fn mock_wire() -> (Wire, tokio::task::JoinHandle<Result<()>>) {
            let (client, server) = tokio::io::duplex(4096);
            let (client_read, client_write) = tokio::io::split(client);
            let (server_read, mut server_write) = tokio::io::split(server);
            let task = tokio::spawn(async move {
                let mut request = String::new();
                BufReader::new(server_read).read_line(&mut request).await?;
                let value: Value = serde_json::from_str(&request)?;
                ensure!(
                    value["method"] == "authenticate" && value["params"]["methodId"] == PERSONAL,
                    "unexpected ACP method"
                );
                server_write
                    .write_all(
                        serde_json::to_string(
                            &json!({"jsonrpc":"2.0","id":value["id"],"result":{}}),
                        )?
                        .as_bytes(),
                    )
                    .await?;
                server_write.write_all(b"\n").await?;
                Ok(())
            });
            (Wire::new(client_read, client_write, 4096), task)
        }
        let (mut wire, server) = mock_wire().await;
        let (tx, mut notices) = mpsc::channel(1);
        tx.send(AuthNotice::Url(Zeroizing::new(
            "https://accounts.google.com/o/oauth2/auth?state=secret".into(),
        )))
        .await?;
        let mut displayed = 0;
        authenticate_protocol(&mut wire, &mut notices, true, |_| displayed += 1).await?;
        assert_eq!(displayed, 1);
        server.await??;

        let (mut wire, server) = mock_wire().await;
        let (tx, mut notices) = mpsc::channel(1);
        tx.send(AuthNotice::Url(Zeroizing::new(
            "https://accounts.google.com/o/oauth2/auth?state=secret".into(),
        )))
        .await?;
        let error = authenticate_protocol(&mut wire, &mut notices, false, |_| {})
            .await
            .unwrap_err();
        assert!(!format!("{error:#}").contains("state=secret"));
        server.await??;

        let (mut wire, server) = mock_wire().await;
        let (_tx, mut notices) = mpsc::channel(1);
        assert!(
            authenticate_protocol(&mut wire, &mut notices, true, |_| {})
                .await
                .is_err()
        );
        server.await??;
        Ok(())
    }
}
