//! Operator-owned Codex account enrollment through the pinned App Server.
//! This path never creates a thread, turn, task, workspace, or broker.
use crate::{
    acp_wire::Wire,
    credential_registry::{
        CredentialStatus, CredentialStore, RepresentationState, RuntimeProvenance,
    },
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
use tokio::process::{Child, Command};

pub const CODEX_INTERFACE: &str = "codex";
pub const CODEX_AUTH_TYPE: &str = "chatgpt-device-code";
pub const CODEX_VERSION: &str = "0.156.0";
pub const CODEX_IMAGE_DIGEST: &str =
    "sha256:5e2441ec351e6dc1ce2100111d0e56a08199b4c9d419150fbd236786a1895895";
pub const CODEX_IMAGE: &str =
    "localhost/orbit-codex@sha256:5e2441ec351e6dc1ce2100111d0e56a08199b4c9d419150fbd236786a1895895";
pub const CODEX_BINARY: &str = "/opt/codex/bin/codex";
pub const CODEX_ARTIFACT: &str = "codex-app-server";
pub const CODEX_BINARY_SHA256: &str =
    "78a11f06e0a2dda42d13fba1d50dc62e8cbdb2d5f69789722f4d4d99b5cdbe30";
pub const CODEX_AUTH_RELATIVE: &str = ".codex/auth.json";
const MAX_AUTH_BYTES: usize = 1024 * 1024;
const MAX_WIRE_BYTES: u64 = 1024 * 1024;
const MAX_PROTOCOL_NOTIFICATIONS: usize = 32;
const LOGIN_TIMEOUT: Duration = Duration::from_secs(900);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexEnrollmentResult {
    pub credential_id: String,
    pub reference: String,
    pub generation: u64,
    pub credential_status: CredentialStatus,
    pub representation_state: RepresentationState,
    pub validation: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceLoginPrompt {
    pub verification_url: String,
    pub user_code: String,
    login_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialStagingFailure {
    CredentialNotFound,
    CredentialRevoked,
    GenerationUnavailable,
    RepresentationMissing,
    RepresentationInvalid,
    SecretBackendUnavailable,
    SecretBackendReadFailed,
    SecretArtifactMissing,
    CredentialStoreUnavailable,
}

impl CredentialStagingFailure {
    pub fn safe_message(self) -> &'static str {
        match self {
            Self::CredentialNotFound => "credential not found",
            Self::CredentialRevoked => "credential is revoked",
            Self::GenerationUnavailable => "current credential generation is unavailable",
            Self::RepresentationMissing => "Codex representation is missing",
            Self::RepresentationInvalid => "Codex representation is not valid",
            Self::SecretBackendUnavailable => "configured SecretBackend is unavailable",
            Self::SecretBackendReadFailed => "SecretBackend artifact could not be loaded",
            Self::SecretArtifactMissing => "SecretBackend artifact is missing",
            Self::CredentialStoreUnavailable => "credential catalog could not be read",
        }
    }
}

impl std::fmt::Display for CredentialStagingFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.safe_message())
    }
}

impl std::error::Error for CredentialStagingFailure {}

fn checked_private_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).context("private Codex directory unavailable")?;
    ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "private Codex directory is unsafe"
    );
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::DirBuilder::new().mode(0o700).create(path)?;
    }
    checked_private_directory(path)
}

fn new_private_home() -> Result<(TempDir, PathBuf)> {
    let root = tempfile::Builder::new()
        .prefix("orbit-codex-enroll-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let home = root.path().join("home");
    fs::DirBuilder::new().mode(0o700).create(&home)?;
    checked_private_directory(root.path())?;
    checked_private_directory(&home)?;
    Ok((root, home))
}

fn auth_path(home: &Path, create: bool) -> Result<PathBuf> {
    checked_private_directory(home)?;
    let directory = home.join(".codex");
    if create {
        create_private_directory(&directory)?;
    } else {
        checked_private_directory(&directory)?;
    }
    Ok(directory.join("auth.json"))
}

fn read_auth_json(home: &Path) -> Result<SecretBytes> {
    let path = auth_path(home, false)?;
    let metadata = fs::symlink_metadata(&path).context("Codex auth representation missing")?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.nlink() == 1
            && metadata.mode() & 0o077 == 0
            && metadata.len() > 0
            && metadata.len() <= MAX_AUTH_BYTES as u64,
        "Codex auth representation is unsafe"
    );
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_AUTH_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_AUTH_BYTES,
        "Codex auth representation exceeds bound"
    );
    SecretBytes::new(bytes)
}

/// Stage exactly one backend blob at the Codex runtime's approved relative path.
pub fn stage_auth_json(home: &Path, secret: &SecretBytes) -> Result<()> {
    ensure!(
        !secret.expose().is_empty() && secret.expose().len() <= MAX_AUTH_BYTES,
        "invalid Codex auth representation size"
    );
    let path = auth_path(home, true)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)?;
    file.write_all(secret.expose())?;
    file.sync_all()?;
    File::open(path.parent().context("Codex auth parent missing")?)?.sync_all()?;
    Ok(())
}

/// Remove the one staged representation after the isolated runtime is gone.
/// Runtime-created non-secret scaffolding remains inside the disposable root.
pub fn clear_staged_auth_json(home: &Path) -> Result<()> {
    let path = home.join(CODEX_AUTH_RELATIVE);
    let metadata =
        fs::symlink_metadata(&path).context("staged Codex auth representation missing")?;
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        "staged Codex auth representation owner changed"
    );
    fs::remove_file(&path)?;
    File::open(path.parent().context("Codex auth parent missing")?)?.sync_all()?;
    ensure!(
        !path.try_exists()?,
        "staged Codex auth representation was not removed"
    );
    Ok(())
}

/// Load one current, validated Codex representation without exposing its locator.
pub async fn registered_auth(
    pool: &PgPool,
    backend: &dyn SecretBackend,
    reference: &str,
) -> Result<SecretBytes> {
    registered_auth_diagnostic(pool, backend, reference)
        .await
        .map_err(anyhow::Error::new)
}

pub async fn registered_auth_diagnostic(
    pool: &PgPool,
    backend: &dyn SecretBackend,
    reference: &str,
) -> std::result::Result<SecretBytes, CredentialStagingFailure> {
    let store = CredentialStore::new(pool);
    let credential = store
        .get(reference)
        .await
        .map_err(|_| CredentialStagingFailure::CredentialStoreUnavailable)?
        .ok_or(CredentialStagingFailure::CredentialNotFound)?;
    if credential.status == CredentialStatus::Revoked {
        return Err(CredentialStagingFailure::CredentialRevoked);
    }
    if credential.status != CredentialStatus::Enrolled {
        return Err(CredentialStagingFailure::GenerationUnavailable);
    }
    if credential.provider != "codex" {
        return Err(CredentialStagingFailure::RepresentationInvalid);
    }
    if credential.secret_backend != backend.backend_id() {
        return Err(CredentialStagingFailure::SecretBackendUnavailable);
    }
    let inspection = store
        .inspect(reference)
        .await
        .map_err(|_| CredentialStagingFailure::CredentialStoreUnavailable)?
        .ok_or(CredentialStagingFailure::GenerationUnavailable)?;
    let view = inspection
        .representations
        .iter()
        .find(|representation| {
            representation.interface == CODEX_INTERFACE
                && representation.generation == credential.generation
                && representation.current_generation
        })
        .ok_or(CredentialStagingFailure::RepresentationMissing)?;
    if view.state != RepresentationState::Stored
        || view.validation != "valid"
        || view.auth_type != CODEX_AUTH_TYPE
    {
        return Err(CredentialStagingFailure::RepresentationInvalid);
    }
    let representation = store
        .representation(&view.id)
        .await
        .map_err(|_| CredentialStagingFailure::CredentialStoreUnavailable)?
        .ok_or(CredentialStagingFailure::GenerationUnavailable)?;
    let locator = representation
        .secret_locator
        .ok_or(CredentialStagingFailure::SecretArtifactMissing)?;
    match backend.exists(locator).await {
        Ok(true) => {}
        Ok(false) => return Err(CredentialStagingFailure::SecretArtifactMissing),
        Err(_) => return Err(CredentialStagingFailure::SecretBackendUnavailable),
    }
    backend
        .read(locator)
        .await
        .map_err(|_| CredentialStagingFailure::SecretBackendReadFailed)
}

fn validate_device_prompt(result: Value) -> Result<DeviceLoginPrompt> {
    ensure!(
        result["type"] == "chatgptDeviceCode",
        "unexpected Codex login method"
    );
    let login_id = result["loginId"]
        .as_str()
        .context("Codex login ID missing")?;
    let verification_url = result["verificationUrl"]
        .as_str()
        .context("Codex verification URL missing")?;
    let user_code = result["userCode"]
        .as_str()
        .context("Codex user code missing")?;
    ensure!(
        !login_id.is_empty()
            && login_id.len() <= 256
            && !login_id.chars().any(char::is_control)
            && (4..=32).contains(&user_code.len())
            && user_code
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        "Codex device login response is invalid"
    );
    let parsed = reqwest::Url::parse(verification_url)
        .map_err(|_| anyhow::anyhow!("Codex verification URL is invalid"))?;
    ensure!(
        parsed.scheme() == "https"
            && parsed.host_str().is_some()
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && verification_url.len() <= 2048,
        "Codex verification URL is unsafe"
    );
    Ok(DeviceLoginPrompt {
        verification_url: verification_url.to_owned(),
        user_code: user_code.to_owned(),
        login_id: login_id.to_owned(),
    })
}

fn allowed_enrollment_notification(method: &str) -> bool {
    matches!(
        method,
        "account/updated"
            | "account/login/completed"
            | "configWarning"
            | "remoteControl/status/changed"
    ) || method.starts_with("codex/event/")
}

async fn correlated_response(wire: &mut Wire, id: &Value) -> Result<Value> {
    for _ in 0..=MAX_PROTOCOL_NOTIFICATIONS {
        let message = wire.read().await?;
        if message.get("method").is_none() {
            ensure!(
                message.get("id") == Some(id),
                "Codex response correlation failed"
            );
            return Wire::result(message, id);
        }
        ensure!(
            message.get("id").is_none(),
            "unexpected Codex server request"
        );
        let method = message["method"]
            .as_str()
            .context("invalid Codex notification")?;
        ensure!(
            allowed_enrollment_notification(method),
            "unexpected Codex notification during enrollment"
        );
    }
    anyhow::bail!("too many Codex enrollment notifications")
}

async fn initialize(wire: &mut Wire) -> Result<()> {
    let id = wire
        .request(
            "initialize",
            json!({"clientInfo":{"name":"orbit-credential-enrollment","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}),
        )
        .await?;
    let result = correlated_response(wire, &id).await?;
    ensure!(
        result["codexHome"] == "/orbit/home/.codex",
        "Codex enrollment HOME mismatch"
    );
    wire.notify("initialized", json!({})).await
}

async fn start_device_login(wire: &mut Wire) -> Result<DeviceLoginPrompt> {
    let id = wire
        .request("account/login/start", json!({"type":"chatgptDeviceCode"}))
        .await?;
    validate_device_prompt(correlated_response(wire, &id).await?)
}

async fn wait_for_login(wire: &mut Wire, expected_login_id: &str) -> Result<()> {
    for _ in 0..=MAX_PROTOCOL_NOTIFICATIONS {
        let message = wire.read().await?;
        ensure!(
            message.get("id").is_none(),
            "unexpected Codex login response"
        );
        let method = message["method"]
            .as_str()
            .context("invalid Codex login notification")?;
        ensure!(
            allowed_enrollment_notification(method),
            "unexpected Codex login notification"
        );
        if method == "account/login/completed" {
            let params = &message["params"];
            ensure!(
                params["loginId"].as_str() == Some(expected_login_id) && params["success"] == true,
                "Codex device login did not complete successfully"
            );
            return Ok(());
        }
    }
    anyhow::bail!("Codex device login completion not observed")
}

async fn account_read(wire: &mut Wire) -> Result<()> {
    let id = wire
        .request("account/read", json!({"refreshToken":false}))
        .await?;
    let result = correlated_response(wire, &id).await?;
    ensure!(
        result["account"]["type"] == "chatgpt",
        "Codex account authentication is unavailable"
    );
    Ok(())
}

struct CodexEnrollmentSession {
    child: Child,
    wire: Wire,
    container_name: String,
    root: TempDir,
    home: PathBuf,
}

impl CodexEnrollmentSession {
    async fn launch(staged: Option<&SecretBytes>) -> Result<Self> {
        let (root, home) = new_private_home()?;
        if let Some(secret) = staged {
            stage_auth_json(&home, secret)?;
        } else {
            let _ = auth_path(&home, true)?;
        }
        for directory in [home.join(".config"), home.join(".cache")] {
            create_private_directory(&directory)?;
        }
        let container_name = format!("orbit-codex-enroll-{}", uuid::Uuid::new_v4());
        let mut command = Command::new("podman");
        command.args([
            "--remote=false",
            "--cgroup-manager=cgroupfs",
            "run",
            "--pull=never",
            "--name",
            &container_name,
            "--label",
            "orbit.managed=true",
            "--label",
            "orbit.credential_enrollment=true",
            "--read-only",
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--pids-limit=64",
            "--init",
            "--log-driver=none",
            "--userns=keep-id",
            "--user",
            &format!("{}:{}", unsafe { libc::getuid() }, unsafe {
                libc::getgid()
            }),
            "--network=pasta",
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
            "CODEX_HOME=/orbit/home/.codex",
            "--env",
            "XDG_CONFIG_HOME=/orbit/home/.config",
            "--env",
            "XDG_CACHE_HOME=/orbit/home/.cache",
            "--env",
            "NO_BROWSER=1",
            "--env",
            "GIT_CONFIG_NOSYSTEM=1",
            "--env",
            "GIT_CONFIG_GLOBAL=/dev/null",
            "--interactive",
            "--entrypoint",
            CODEX_BINARY,
            CODEX_IMAGE,
            "-c",
            "cli_auth_credentials_store=\"file\"",
            "app-server",
        ]);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .context("cannot start pinned Codex enrollment runtime")?;
        let stdout = child
            .stdout
            .take()
            .context("Codex App Server stdout unavailable")?;
        let stdin = child
            .stdin
            .take()
            .context("Codex App Server stdin unavailable")?;
        Ok(Self {
            child,
            wire: Wire::new(stdout, stdin, MAX_WIRE_BYTES).codex(),
            container_name,
            root,
            home,
        })
    }

    async fn stop(&mut self) -> Result<()> {
        self.child.start_kill().ok();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
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
        .context("Codex enrollment cleanup timed out")?
        .context("Codex enrollment cleanup failed")?;
        ensure!(
            output.status.success(),
            "Codex enrollment cleanup unconfirmed"
        );
        Ok(())
    }
}

async fn verify_pinned_runtime() -> Result<()> {
    ensure!(
        unsafe { libc::geteuid() } != 0,
        "Codex enrollment requires rootless Podman"
    );
    let inspect = Command::new("podman")
        .args([
            "--remote=false",
            "--cgroup-manager=cgroupfs",
            "image",
            "inspect",
            "--format",
            "{{json .RepoDigests}}",
            CODEX_IMAGE,
        ])
        .output()
        .await
        .context("pinned Codex image inspection failed")?;
    ensure!(
        inspect.status.success() && inspect.stdout.len() <= 8192,
        "pinned Codex image unavailable"
    );
    let digests: Vec<String> = serde_json::from_slice(&inspect.stdout)
        .map_err(|_| anyhow::anyhow!("pinned Codex image metadata invalid"))?;
    ensure!(
        digests
            .iter()
            .any(|digest| digest.ends_with(CODEX_IMAGE_DIGEST)),
        "pinned Codex image digest mismatch"
    );
    let version = Command::new("podman")
        .args([
            "--remote=false",
            "--cgroup-manager=cgroupfs",
            "run",
            "--rm",
            "--pull=never",
            "--network=none",
            "--read-only",
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--pids-limit=16",
            "--userns=keep-id",
            "--user",
            &format!("{}:{}", unsafe { libc::getuid() }, unsafe {
                libc::getgid()
            }),
            "--tmpfs",
            "/tmp:rw,nosuid,nodev,size=16777216",
            "--workdir",
            "/tmp",
            "--env",
            "HOME=/tmp",
            "--env",
            "CODEX_HOME=/tmp",
            "--entrypoint",
            CODEX_BINARY,
            CODEX_IMAGE,
            "--version",
        ])
        .output()
        .await
        .context("pinned Codex version check failed")?;
    ensure!(
        version.status.success()
            && String::from_utf8_lossy(&version.stdout).trim()
                == format!("codex-cli {CODEX_VERSION}"),
        "pinned Codex version mismatch"
    );
    Ok(())
}

/// Perform one operator-interactive device-code enrollment. Callers must have
/// separately authorized the live login before invoking this function.
pub async fn enroll_codex_chatgpt(pool: &PgPool, reference: &str) -> Result<CodexEnrollmentResult> {
    ensure!(
        crate::credential_registry::valid_reference(reference),
        "invalid credential reference"
    );
    verify_pinned_runtime().await?;
    let backend = LocalPrivateSecretBackend::default_for_operator()?;
    let store = CredentialStore::new(pool);
    let provenance = RuntimeProvenance {
        artifact: CODEX_ARTIFACT.to_owned(),
        version: CODEX_VERSION.to_owned(),
        sha256: CODEX_BINARY_SHA256.to_owned(),
        provenance: "pinned-build".to_owned(),
    };
    let (credential, pending) = if let Some(existing) = store.get(reference).await? {
        ensure!(
            existing.provider == "codex"
                && existing.status == CredentialStatus::Pending
                && existing.generation == 1
                && existing.auth_type == CODEX_AUTH_TYPE
                && existing.secret_backend == LOCAL_PRIVATE_ID
                && existing.secret_locator.is_none(),
            "existing Codex credential is not a resumable pending generation"
        );
        let inspection = store
            .inspect(reference)
            .await?
            .context("pending Codex credential disappeared")?;
        ensure!(
            inspection
                .representations
                .iter()
                .all(
                    |representation| representation.generation != existing.generation
                        || representation.interface == CODEX_INTERFACE
                ),
            "pending Codex credential has an unexpected current representation"
        );
        let matching = inspection.representations.iter().find(|representation| {
            representation.generation == existing.generation
                && representation.current_generation
                && representation.interface == CODEX_INTERFACE
        });
        let pending = if let Some(view) = matching {
            ensure!(
                view.state == RepresentationState::Pending && view.auth_type == CODEX_AUTH_TYPE,
                "existing Codex representation is not resumable"
            );
            let representation = store
                .representation(&view.id)
                .await?
                .context("pending Codex representation disappeared")?;
            let locator = representation
                .secret_locator
                .context("pending Codex locator missing")?;
            ensure!(
                !backend.exists(locator).await?,
                "pending Codex representation already has a secret; refusing replacement"
            );
            representation
        } else {
            store
                .prepare_representation_with_metadata(
                    reference,
                    CODEX_INTERFACE,
                    CODEX_AUTH_TYPE,
                    &[],
                    backend.backend_id(),
                    Some(&provenance),
                )
                .await?
        };
        ensure!(
            pending.runtime_provenance.as_ref() == Some(&provenance),
            "pending Codex runtime provenance differs from the pinned runtime"
        );
        (existing, pending)
    } else {
        let credential = store
            .create("codex", reference, None, CODEX_AUTH_TYPE, LOCAL_PRIVATE_ID)
            .await?;
        let pending = store
            .prepare_representation_with_metadata(
                reference,
                CODEX_INTERFACE,
                CODEX_AUTH_TYPE,
                &[],
                backend.backend_id(),
                Some(&provenance),
            )
            .await?;
        (credential, pending)
    };

    let mut enrollment = CodexEnrollmentSession::launch(None).await?;
    let interaction = async {
        initialize(&mut enrollment.wire).await?;
        let prompt = start_device_login(&mut enrollment.wire).await?;
        // These short-lived values have exactly one sink: the operator terminal.
        eprintln!(
            "Open this URL in your browser:\n{}\nEnter this one-time code:\n{}\nWaiting for Codex authentication...",
            prompt.verification_url, prompt.user_code
        );
        tokio::time::timeout(
            LOGIN_TIMEOUT,
            wait_for_login(&mut enrollment.wire, &prompt.login_id),
        )
        .await
        .context("Codex device login timed out")??;
        account_read(&mut enrollment.wire).await
    }
    .await;
    enrollment.stop().await?;
    interaction?;
    let captured = read_auth_json(&enrollment.home)?;
    let locator = pending
        .secret_locator
        .context("pending Codex locator missing")?;
    backend.create(locator, captured).await?;
    enrollment
        .root
        .close()
        .context("Codex enrollment HOME cleanup failed")?;

    let stored = backend.read(locator).await?;
    let mut validation = CodexEnrollmentSession::launch(Some(&stored)).await?;
    let reuse = async {
        initialize(&mut validation.wire).await?;
        account_read(&mut validation.wire).await
    }
    .await;
    validation.stop().await?;
    reuse?;
    // No login/start request exists on the validation path. Do not write a
    // possibly refreshed staged file back until a separate refresh policy exists.
    validation
        .root
        .close()
        .context("Codex validation HOME cleanup failed")?;
    verify_pinned_runtime().await?;

    let finalized = store
        .finalize_validated_representation(&backend, &pending.id)
        .await?;
    ensure!(
        finalized.state == RepresentationState::Stored && finalized.last_validated_at_ms.is_some(),
        "Codex representation validation was not recorded"
    );
    let enrolled = store
        .get(reference)
        .await?
        .context("Codex credential disappeared")?;
    ensure!(
        enrolled.id == credential.id
            && enrolled.generation == credential.generation
            && enrolled.status == CredentialStatus::Enrolled,
        "Codex credential finalization incomplete"
    );
    Ok(CodexEnrollmentResult {
        credential_id: enrolled.id,
        reference: enrolled.reference,
        generation: enrolled.generation,
        credential_status: enrolled.status,
        representation_state: finalized.state,
        validation: "valid",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[test]
    fn device_prompt_is_bounded_https_and_redacted_by_type() -> Result<()> {
        let prompt = validate_device_prompt(json!({
            "type":"chatgptDeviceCode",
            "loginId":"login-1",
            "verificationUrl":"https://auth.openai.com/codex/device",
            "userCode":"ABCD-EFGH"
        }))?;
        assert_eq!(prompt.login_id, "login-1");
        assert!(validate_device_prompt(json!({
            "type":"chatgptDeviceCode","loginId":"x","verificationUrl":"http://example.com","userCode":"ABCD"
        })).is_err());
        assert!(validate_device_prompt(json!({
            "type":"chatgpt","loginId":"x","verificationUrl":"https://example.com","userCode":"ABCD"
        })).is_err());
        Ok(())
    }

    #[test]
    fn capture_and_stage_only_auth_json() -> Result<()> {
        let (_root, home) = new_private_home()?;
        let auth = auth_path(&home, true)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&auth)?;
        file.write_all(b"synthetic-codex-auth")?;
        fs::DirBuilder::new()
            .mode(0o700)
            .create(home.join(".codex/sessions"))?;
        let mut history = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(home.join(".codex/sessions/history.jsonl"))?;
        history.write_all(b"must-not-copy")?;
        let captured = read_auth_json(&home)?;
        assert!(!format!("{captured:?}").contains("synthetic-codex-auth"));
        let (_fresh_root, fresh_home) = new_private_home()?;
        stage_auth_json(&fresh_home, &captured)?;
        assert!(fresh_home.join(CODEX_AUTH_RELATIVE).is_file());
        assert!(!fresh_home.join(".codex/sessions").exists());
        assert_eq!(
            fs::metadata(fresh_home.join(CODEX_AUTH_RELATIVE))?
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        Ok(())
    }

    #[tokio::test]
    async fn mocked_device_login_and_account_reuse_use_no_inference_methods() -> Result<()> {
        let (client, server) = tokio::io::duplex(32 * 1024);
        let (client_read, client_write) = tokio::io::split(client);
        let (server_read, mut server_write) = tokio::io::split(server);
        let server = tokio::spawn(async move {
            let mut lines = BufReader::new(server_read).lines();
            let mut methods = Vec::new();
            while let Some(line) = lines.next_line().await? {
                let request: Value = serde_json::from_str(&line)?;
                let method = request["method"]
                    .as_str()
                    .context("method missing")?
                    .to_owned();
                methods.push(method.clone());
                match method.as_str() {
                    "initialize" => {
                        server_write.write_all(serde_json::to_string(&json!({"id":request["id"],"result":{"codexHome":"/orbit/home/.codex"}}))?.as_bytes()).await?;
                        server_write.write_all(b"\n").await?;
                    }
                    "initialized" => {}
                    "account/login/start" => {
                        ensure!(
                            request["params"]["type"] == "chatgptDeviceCode",
                            "wrong login type"
                        );
                        server_write.write_all(serde_json::to_string(&json!({"id":request["id"],"result":{"type":"chatgptDeviceCode","loginId":"login-1","verificationUrl":"https://auth.openai.com/codex/device","userCode":"ABCD-EFGH"}}))?.as_bytes()).await?;
                        server_write.write_all(b"\n").await?;
                        server_write.write_all(serde_json::to_string(&json!({"method":"account/login/completed","params":{"loginId":"login-1","success":true,"error":null}}))?.as_bytes()).await?;
                        server_write.write_all(b"\n").await?;
                    }
                    "account/read" => {
                        ensure!(
                            request["params"]["refreshToken"] == false,
                            "refresh requested"
                        );
                        server_write.write_all(serde_json::to_string(&json!({"id":request["id"],"result":{"account":{"type":"chatgpt","email":null,"planType":"unknown"},"requiresOpenaiAuth":true}}))?.as_bytes()).await?;
                        server_write.write_all(b"\n").await?;
                        break;
                    }
                    _ => anyhow::bail!("unexpected method"),
                }
            }
            Ok::<_, anyhow::Error>(methods)
        });
        let mut wire = Wire::new(client_read, client_write, MAX_WIRE_BYTES).codex();
        initialize(&mut wire).await?;
        let prompt = start_device_login(&mut wire).await?;
        wait_for_login(&mut wire, &prompt.login_id).await?;
        account_read(&mut wire).await?;
        let methods = server.await??;
        assert_eq!(
            methods,
            [
                "initialize",
                "initialized",
                "account/login/start",
                "account/read"
            ]
        );
        assert!(
            methods
                .iter()
                .all(|method| !method.starts_with("thread/") && !method.starts_with("turn/"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn fresh_reuse_calls_account_read_without_starting_login() -> Result<()> {
        let (client, server) = tokio::io::duplex(8192);
        let (client_read, client_write) = tokio::io::split(client);
        let (server_read, mut server_write) = tokio::io::split(server);
        let server = tokio::spawn(async move {
            let mut lines = BufReader::new(server_read).lines();
            let request: Value = serde_json::from_str(
                &lines
                    .next_line()
                    .await?
                    .context("account request missing")?,
            )?;
            ensure!(
                request["method"] == "account/read",
                "unexpected reuse method"
            );
            server_write
                .write_all(
                    serde_json::to_string(&json!({
                        "id":request["id"],
                        "result":{"account":{"type":"chatgpt","email":null,"planType":"unknown"},"requiresOpenaiAuth":true}
                    }))?
                    .as_bytes(),
                )
                .await?;
            server_write.write_all(b"\n").await?;
            Ok::<_, anyhow::Error>(())
        });
        let mut wire = Wire::new(client_read, client_write, MAX_WIRE_BYTES).codex();
        account_read(&mut wire).await?;
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn failed_or_mismatched_login_completion_is_rejected() -> Result<()> {
        for params in [
            json!({"loginId":"login-1","success":false,"error":"cancelled"}),
            json!({"loginId":"other-login","success":true,"error":null}),
        ] {
            let (client, mut server) = tokio::io::duplex(4096);
            server
                .write_all(
                    format!(
                        "{}\n",
                        serde_json::to_string(
                            &json!({"method":"account/login/completed","params":params})
                        )?
                    )
                    .as_bytes(),
                )
                .await?;
            let (client_read, client_write) = tokio::io::split(client);
            let mut wire = Wire::new(client_read, client_write, MAX_WIRE_BYTES).codex();
            assert!(wait_for_login(&mut wire, "login-1").await.is_err());
        }
        Ok(())
    }
}
