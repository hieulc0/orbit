//! Bounded operational status read through the pinned, isolated Codex App Server.
//! No ACP session, Attempt/repository workspace, broker, or model turn exists on
//! this path. Auth staging may create an empty directory named `workspace`
//! inside the disposable private control HOME; it is not a coding workspace.
use crate::{
    acp_contract::{
        Accounting, Auth, AuthMode, Descriptor, FilesystemPolicy, Limits, ModelPolicy,
        SecurityProfile, TerminalPolicy,
    },
    acp_process::{AuthLease, command_for_runtime, preflight_codex_launch, resolve_image},
    acp_runtime::{Adapter, AgentNetwork, AuthStore, Launch, Runtime},
    acp_wire::{StreamClosed, Wire},
    agent::{Binding, Budget},
    availability::{AvailabilitySnapshot, ExecutionResourceIdentity},
    codex_credential_enrollment::{clear_staged_auth_json, registered_auth, stage_auth_json},
    credential_registry::CredentialStore,
    provider_scope::fingerprint,
    provider_status::codex_rate_limits_snapshot,
    secret_backend::SecretBackend,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};

const MAX_WIRE_BYTES: u64 = 65_536;
const MAX_NOTIFICATIONS: usize = 8;
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Construct the fixed, previously qualified Codex status runtime identity for
/// a catalog credential. The catalog-backed probe stages SecretBackend data
/// directly; `auth.path` is an unused validation placeholder, never a source.
pub fn cataloged_codex_runtime(
    credential: &crate::credential_registry::Credential,
) -> Result<(Runtime, ExecutionResourceIdentity)> {
    use crate::codex_credential_enrollment as enrolled;

    ensure!(
        credential.provider == "codex"
            && credential.status == crate::credential_registry::CredentialStatus::Enrolled,
        "Codex credential is not enrolled"
    );
    let binding_name = "codex-status-v1";
    let model = "gpt-6-luna";
    let launch = Launch {
        adapter: Adapter::Codex,
        image: enrolled::CODEX_IMAGE.into(),
        command: vec![enrolled::CODEX_BINARY.into(), "app-server".into()],
        agent_name: "orbit-codex-acp".into(),
        agent_version: "1".into(),
        binary_revision: enrolled::CODEX_VERSION.into(),
        cpu_millis: 1000,
        memory_mib: 512,
        network: AgentNetwork::Host,
    };
    let auth = Auth {
        source: "codex".into(),
        owner: credential.reference.clone(),
        account_class: "chatgpt".into(),
        mode: AuthMode::LocalSession,
    };
    let descriptor = Descriptor {
        agent_id: "codex".into(),
        agent_revision: crate::codex_bridge::REVISION.into(),
        launch_digest: launch.digest()?,
        protocol_version: 1,
        auth: auth.clone(),
        security_profile: SecurityProfile::Trusted,
        filesystem_policy: FilesystemPolicy::AttemptWorkspace,
        terminal_policy: TerminalPolicy::WorkspaceSupervisor,
        model_policy: ModelPolicy::Exact,
        accounting: Accounting::ExecutionOnly,
        max_limits: Limits {
            prompt_turns: 1,
            broker_calls: 0,
            reported_tool_calls: 0,
            turn_timeout_seconds: 60,
            terminal_timeout_seconds: 30,
            terminal_runtime_seconds: 0,
            output_bytes: 4096,
        },
    };
    let runtime = Runtime {
        binding_name: binding_name.into(),
        binding: Binding {
            model: Some(model.into()),
            runtime: "agent.acp-codex-status-v1".into(),
            tools: Default::default(),
            permissions: Vec::new(),
            max_budget: Budget {
                tokens: None,
                cost_microusd: None,
                calls: 1,
            },
            max_delegations: 0,
            acp: Some(descriptor),
        },
        launch,
        auth: AuthStore {
            path: PathBuf::from("/orbit/catalog-auth-not-used"),
            source: auth.source,
            owner: auth.owner,
            account_class: auth.account_class,
            files: [("auth.json".into(), enrolled::CODEX_AUTH_RELATIVE.into())].into(),
            scopes: Vec::new(),
        },
        reasoning_effort: None,
    };
    runtime.validate()?;
    let resource = ExecutionResourceIdentity {
        runtime: crate::availability::RuntimeIdentity {
            family: "acp".into(),
            adapter: "codex_bridge".into(),
            binding: binding_name.into(),
            image_digest: enrolled::CODEX_IMAGE_DIGEST.into(),
            agent_revision: enrolled::CODEX_VERSION.into(),
            adapter_version: "1".into(),
        },
        credential: credential.identity(),
        model: model.into(),
        reasoning_effort: None,
    };
    validate_binding(&runtime, &resource)?;
    Ok((runtime, resource))
}

/// Structural evidence only. No account ID, auth bytes, provider payload or path.
/// `repository_effects` excludes private control-HOME/auth scaffolding and
/// refers only to repository materialization or mutation.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ProbeReceipt {
    pub runtime_launched: bool,
    pub isolated_auth_staged: bool,
    pub protocol_initialized: bool,
    pub authenticated_account_present: bool,
    pub status_request_sent: bool,
    pub correlated_status_response: bool,
    /// Exact match to the private operator-supplied ID or confirmed fingerprint.
    /// `account/read` has no documented matching field.
    pub account_scope_matched: bool,
    pub model_thread_created: bool,
    pub model_turn_started: bool,
    pub broker_callbacks: u8,
    pub broker_filesystem_callbacks: u8,
    pub broker_terminal_callbacks: u8,
    pub repository_effects: u8,
    pub cleanup_confirmed: bool,
}

/// The catalog-backed equivalent of [`probe_once`]. It stages only the
/// validated `codex` representation from SecretBackend and deliberately does
/// not copy runtime refresh writes back until a refresh policy is approved.
pub struct CatalogCredentialSource<'a> {
    pub pool: &'a PgPool,
    pub backend: &'a dyn SecretBackend,
    pub reference: &'a str,
}

pub async fn probe_cataloged_once(
    source: CatalogCredentialSource<'_>,
    runtime: &Runtime,
    resource: &ExecutionResourceIdentity,
    binding: ProbeBinding<'_>,
    control_root: &Path,
    ttl: Duration,
) -> std::result::Result<ProbeOutcome, ProbeFailure> {
    let mut receipt = ProbeReceipt::default();
    let failure = |kind, receipt: &ProbeReceipt| ProbeFailure {
        kind,
        control_root_diagnostic: None,
        receipt: receipt.clone(),
    };
    validate_binding(runtime, resource)
        .map_err(|_| failure(ProbeFailureKind::InvalidBinding, &receipt))?;
    if resource.credential.reference != source.reference
        || matches!(binding, ProbeBinding::ExpectedAccountId(id) if id.is_empty() || id.len() > 256)
    {
        return Err(failure(ProbeFailureKind::InvalidBinding, &receipt));
    }
    if !(1..=300).contains(&ttl.as_secs()) {
        return Err(failure(ProbeFailureKind::InvalidPolicy, &receipt));
    }
    let root = validate_control_root(control_root).map_err(|diagnostic| ProbeFailure {
        kind: ProbeFailureKind::InvalidControlRoot,
        control_root_diagnostic: Some(diagnostic),
        receipt: receipt.clone(),
    })?;
    preflight_codex_launch(&runtime.launch)
        .await
        .map_err(|_| failure(ProbeFailureKind::RuntimeLaunch, &receipt))?;
    let credential = CredentialStore::new(source.pool)
        .get(source.reference)
        .await
        .map_err(|_| failure(ProbeFailureKind::CredentialUnavailable, &receipt))?
        .ok_or_else(|| failure(ProbeFailureKind::CredentialUnavailable, &receipt))?;
    if credential.provider != resource.credential.provider
        || credential.generation.to_string() != resource.credential.generation
        || resource.credential.catalog_id.as_deref() != Some(credential.id.as_str())
    {
        return Err(failure(ProbeFailureKind::InvalidBinding, &receipt));
    }
    let secret = registered_auth(source.pool, source.backend, source.reference)
        .await
        .map_err(|_| failure(ProbeFailureKind::CredentialUnavailable, &receipt))?;
    let probe_id = crate::model::id();
    let home = root.join(format!("status-home-{probe_id}"));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&home)
        .map_err(|_| failure(ProbeFailureKind::CredentialUnavailable, &receipt))?;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(home.join("workspace"))
        .map_err(|_| failure(ProbeFailureKind::CredentialUnavailable, &receipt))?;
    stage_auth_json(&home, &secret)
        .map_err(|_| failure(ProbeFailureKind::CredentialUnavailable, &receipt))?;
    drop(secret);
    receipt.isolated_auth_staged = true;
    let name = format!("orbit-status-{probe_id}");
    let read: std::result::Result<Value, ProbeFailureKind> = async {
        let image = resolve_image(&runtime.launch.image)
            .await
            .map_err(|_| ProbeFailureKind::RuntimeLaunch)?;
        let mut command = command_for_runtime(
            runtime,
            &home,
            &name,
            &image,
            "orbit.status_probe",
            &probe_id,
        )
        .map_err(|_| ProbeFailureKind::RuntimeLaunch)?;
        command.stderr(Stdio::null());
        let mut child = command
            .spawn()
            .map_err(|_| ProbeFailureKind::RuntimeLaunch)?;
        receipt.runtime_launched = true;
        let input = child.stdin.take().ok_or(ProbeFailureKind::RuntimeLaunch)?;
        let output = child.stdout.take().ok_or(ProbeFailureKind::RuntimeLaunch)?;
        let mut wire = Wire::new(output, input, MAX_WIRE_BYTES).codex();
        let result = tokio::time::timeout(PROBE_TIMEOUT, protocol(&mut wire, &mut receipt)).await;
        drop(wire);
        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        if child
            .try_wait()
            .map_err(|_| ProbeFailureKind::RuntimeLaunch)?
            .is_none()
        {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        }
        match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) if error.downcast_ref::<StreamClosed>().is_some() => {
                Err(ProbeFailureKind::UnexpectedEof)
            }
            Ok(Err(error)) if error.downcast_ref::<AuthRequired>().is_some() => {
                Err(ProbeFailureKind::Authentication)
            }
            Ok(Err(_)) => Err(ProbeFailureKind::Protocol),
            Err(_) => Err(ProbeFailureKind::Timeout),
        }
    }
    .await;
    crate::container::remove("podman", &name)
        .await
        .map_err(|_| failure(ProbeFailureKind::CleanupUncertain, &receipt))?;
    clear_staged_auth_json(&home)
        .map_err(|_| failure(ProbeFailureKind::CleanupUncertain, &receipt))?;
    receipt.cleanup_confirmed = true;
    let value = read.map_err(|kind| failure(kind, &receipt))?;
    let observed_at_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| failure(ProbeFailureKind::InvalidPolicy, &receipt))?
            .as_millis(),
    )
    .map_err(|_| failure(ProbeFailureKind::InvalidPolicy, &receipt))?;
    let expires_at_ms = observed_at_ms
        + i64::try_from(ttl.as_millis())
            .map_err(|_| failure(ProbeFailureKind::InvalidPolicy, &receipt))?;
    let (snapshot, provider_scope_fingerprint, matched) =
        normalize_observation(resource, binding, &value, observed_at_ms, expires_at_ms)
            .map_err(|_| failure(ProbeFailureKind::Protocol, &receipt))?;
    receipt.account_scope_matched = matched;
    Ok(ProbeOutcome {
        snapshot,
        receipt,
        provider_scope_fingerprint,
    })
}

pub struct ProbeOutcome {
    pub snapshot: AvailabilitySnapshot,
    pub receipt: ProbeReceipt,
    pub provider_scope_fingerprint: Option<String>,
}

/// Only the expected-ID path or a previously confirmed fingerprint may
/// produce trusted availability. Enrollment is deliberately observation-only.
#[derive(Clone, Copy)]
pub enum ProbeBinding<'a> {
    ExpectedAccountId(&'a str),
    Enroll,
    ConfirmedFingerprint(&'a str),
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeFailureKind {
    InvalidBinding,
    InvalidControlRoot,
    InvalidPolicy,
    CredentialUnavailable,
    RuntimeLaunch,
    Authentication,
    Protocol,
    UnexpectedEof,
    Timeout,
    CleanupUncertain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlRootFailureReason {
    NotAbsolute,
    NonCanonicalPath,
    NotFound,
    MetadataUnavailable,
    SymlinkRejected,
    NotDirectory,
    CanonicalizationFailed,
    WrongOwner,
    PermissionsTooBroad,
    OwnerPermissionsInsufficient,
    ParentUnavailable,
    ParentSymlinkRejected,
    ParentNotDirectory,
    ParentInsecure,
}

/// Safe filesystem metadata only. Paths are deliberately omitted.
#[derive(Debug, Clone, Serialize)]
pub struct ControlRootDiagnostic {
    pub reason: ControlRootFailureReason,
    pub observed_uid: Option<u32>,
    pub expected_uid: u32,
    pub mode: Option<u32>,
    pub parent_uid: Option<u32>,
    pub parent_mode: Option<u32>,
}

/// Fixed-category failure evidence, without provider text or auth material.
#[derive(Debug, Clone, Serialize)]
pub struct ProbeFailure {
    pub kind: ProbeFailureKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control_root_diagnostic: Option<ControlRootDiagnostic>,
    pub receipt: ProbeReceipt,
}
impl std::fmt::Display for ProbeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(diagnostic) = &self.control_root_diagnostic {
            write!(
                f,
                "Codex status probe failed: {:?} ({:?}, uid={:?}, expected_uid={}, mode={:?}, parent_uid={:?}, parent_mode={:?})",
                self.kind,
                diagnostic.reason,
                diagnostic.observed_uid,
                diagnostic.expected_uid,
                diagnostic.mode.map(|mode| format!("{mode:#o}")),
                diagnostic.parent_uid,
                diagnostic.parent_mode.map(|mode| format!("{mode:#o}")),
            )
        } else {
            write!(f, "Codex status probe failed: {:?}", self.kind)
        }
    }
}
impl std::error::Error for ProbeFailure {}

/// Create a private control root beneath the platform temporary directory.
/// The shared parent may be sticky/world-writable (for example `/tmp`); the
/// child containing auth/runtime state must itself be owner-only.
pub fn private_control_tempdir() -> std::io::Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    builder.permissions(std::fs::Permissions::from_mode(0o700));
    builder.tempdir()
}

fn diagnostic(
    reason: ControlRootFailureReason,
    expected_uid: u32,
    metadata: Option<&std::fs::Metadata>,
    parent: Option<&std::fs::Metadata>,
) -> ControlRootDiagnostic {
    ControlRootDiagnostic {
        reason,
        observed_uid: metadata.map(MetadataExt::uid),
        expected_uid,
        mode: metadata.map(|metadata| metadata.mode() & 0o7777),
        parent_uid: parent.map(MetadataExt::uid),
        parent_mode: parent.map(|metadata| metadata.mode() & 0o7777),
    }
}

fn validate_private_directory(
    metadata: &std::fs::Metadata,
    expected_uid: u32,
) -> Result<(), ControlRootFailureReason> {
    if metadata.uid() != expected_uid {
        return Err(ControlRootFailureReason::WrongOwner);
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(ControlRootFailureReason::PermissionsTooBroad);
    }
    if metadata.mode() & 0o700 != 0o700 {
        return Err(ControlRootFailureReason::OwnerPermissionsInsufficient);
    }
    Ok(())
}

fn validate_control_parent(
    metadata: &std::fs::Metadata,
    expected_uid: u32,
) -> Result<(), ControlRootFailureReason> {
    validate_control_parent_fields(metadata.uid(), metadata.mode(), expected_uid)
}

fn validate_control_parent_fields(
    uid: u32,
    mode: u32,
    expected_uid: u32,
) -> Result<(), ControlRootFailureReason> {
    let shared_writable = mode & 0o022 != 0;
    let sticky = mode & 0o1000 != 0;
    let trusted_owner = uid == expected_uid || uid == 0;
    if (shared_writable && !sticky) || (!trusted_owner && !(shared_writable && sticky)) {
        return Err(ControlRootFailureReason::ParentInsecure);
    }
    Ok(())
}

fn validate_control_root(path: &Path) -> Result<PathBuf, ControlRootDiagnostic> {
    let expected_uid = unsafe { libc::geteuid() };
    let reject = |reason, metadata, parent| Err(diagnostic(reason, expected_uid, metadata, parent));
    if !path.is_absolute() {
        return reject(ControlRootFailureReason::NotAbsolute, None, None);
    }
    if path
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return reject(ControlRootFailureReason::NonCanonicalPath, None, None);
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return reject(ControlRootFailureReason::NotFound, None, None);
        }
        Err(_) => return reject(ControlRootFailureReason::MetadataUnavailable, None, None),
    };
    if metadata.file_type().is_symlink() {
        return reject(
            ControlRootFailureReason::SymlinkRejected,
            Some(&metadata),
            None,
        );
    }
    if !metadata.is_dir() {
        return reject(
            ControlRootFailureReason::NotDirectory,
            Some(&metadata),
            None,
        );
    }
    let canonical = match path.canonicalize() {
        Ok(canonical) => canonical,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return reject(ControlRootFailureReason::NotFound, Some(&metadata), None);
        }
        Err(_) => {
            return reject(
                ControlRootFailureReason::CanonicalizationFailed,
                Some(&metadata),
                None,
            );
        }
    };
    if canonical != path {
        return reject(
            ControlRootFailureReason::SymlinkRejected,
            Some(&metadata),
            None,
        );
    }
    if let Err(reason) = validate_private_directory(&metadata, expected_uid) {
        return reject(reason, Some(&metadata), None);
    }

    let parent_path = match canonical.parent() {
        Some(parent) => parent,
        None => {
            return reject(
                ControlRootFailureReason::ParentUnavailable,
                Some(&metadata),
                None,
            );
        }
    };
    let parent_metadata = match std::fs::symlink_metadata(parent_path) {
        Ok(metadata) => metadata,
        Err(_) => {
            return reject(
                ControlRootFailureReason::ParentUnavailable,
                Some(&metadata),
                None,
            );
        }
    };
    if parent_metadata.file_type().is_symlink() {
        return reject(
            ControlRootFailureReason::ParentSymlinkRejected,
            Some(&metadata),
            Some(&parent_metadata),
        );
    }
    if !parent_metadata.is_dir() {
        return reject(
            ControlRootFailureReason::ParentNotDirectory,
            Some(&metadata),
            Some(&parent_metadata),
        );
    }
    if let Err(reason) = validate_control_parent(&parent_metadata, expected_uid) {
        return reject(reason, Some(&metadata), Some(&parent_metadata));
    }
    Ok(canonical)
}

#[derive(Debug)]
struct AuthRequired;
impl std::fmt::Display for AuthRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Codex status authentication required")
    }
}
impl std::error::Error for AuthRequired {}

fn validate_binding(runtime: &Runtime, resource: &ExecutionResourceIdentity) -> Result<()> {
    runtime.validate()?;
    resource.validate()?;
    ensure!(
        runtime.launch.adapter == Adapter::Codex,
        "Codex status runtime required"
    );
    let image_digest = runtime.launch.image.rsplit('@').next().unwrap_or_default();
    ensure!(
        resource.runtime.family == "acp"
            && resource.runtime.adapter == "codex_bridge"
            && resource.runtime.binding == runtime.binding_name
            && resource.runtime.image_digest == image_digest
            && resource.runtime.agent_revision == runtime.launch.binary_revision
            && resource.runtime.adapter_version == runtime.launch.agent_version
            && resource.credential.provider == runtime.auth.source
            && resource.credential.reference == runtime.auth.owner
            && runtime.binding.model.as_deref() == Some(resource.model.as_str())
            && resource.reasoning_effort == runtime.reasoning_effort,
        "status resource does not match pinned runtime/credential binding"
    );
    Ok(())
}

async fn call(wire: &mut Wire, method: &str, params: Value) -> Result<Value> {
    let id = wire.request(method, params).await?;
    for _ in 0..=MAX_NOTIFICATIONS {
        let message = wire.read().await.context("Codex status response missing")?;
        if message.get("method").is_none() {
            ensure!(
                message.get("id") == Some(&id),
                "status response correlation failed"
            );
            return Wire::result(message, &id);
        }
        // No callbacks exist on this path. Reject every server request, even
        // one masquerading as an unrelated notification with an ID.
        ensure!(
            message.get("id").is_none(),
            "unexpected status server request"
        );
        let notification = message["method"]
            .as_str()
            .context("invalid status notification")?;
        ensure!(
            !notification.starts_with("thread/")
                && !notification.starts_with("turn/")
                && !notification.starts_with("item/")
                && notification != "error",
            "unexpected inference/protocol notification during status probe"
        );
    }
    anyhow::bail!("too many status notifications")
}

/// This is deliberately distinct from `codex_session::run`: no `thread/start`,
/// `turn/start`, ACP peer, tool registration, or broker callback is possible.
pub async fn protocol(wire: &mut Wire, receipt: &mut ProbeReceipt) -> Result<Value> {
    let initialized = call(
        wire,
        "initialize",
        json!({"clientInfo":{"name":"orbit","version":"1"},"capabilities":{"experimentalApi":true}}),
    )
    .await?;
    ensure!(
        initialized["codexHome"] == "/orbit/home/.codex",
        "Codex status HOME mismatch"
    );
    receipt.protocol_initialized = true;
    wire.notify("initialized", json!({})).await?;
    let account = call(wire, "account/read", json!({"refreshToken":false})).await?;
    if account["account"].is_null() && account["requiresOpenaiAuth"] != false {
        return Err(AuthRequired.into());
    }
    receipt.authenticated_account_present = !account["account"].is_null();
    receipt.status_request_sent = true;
    let status = call(wire, "account/rateLimits/read", json!({})).await?;
    receipt.correlated_status_response = true;
    Ok(status)
}

/// One isolated, single-credential status read. The caller may persist the
/// returned snapshot only after confirmed cleanup; errors never return READY.
pub async fn probe_once(
    runtime: &Runtime,
    resource: &ExecutionResourceIdentity,
    binding: ProbeBinding<'_>,
    control_root: &Path,
    ttl: Duration,
) -> std::result::Result<ProbeOutcome, ProbeFailure> {
    let mut receipt = ProbeReceipt::default();
    let failure = |kind, receipt: &ProbeReceipt| ProbeFailure {
        kind,
        control_root_diagnostic: None,
        receipt: receipt.clone(),
    };
    validate_binding(runtime, resource)
        .map_err(|_| failure(ProbeFailureKind::InvalidBinding, &receipt))?;
    if matches!(binding, ProbeBinding::ExpectedAccountId(id) if id.is_empty() || id.len() > 256) {
        return Err(failure(ProbeFailureKind::InvalidBinding, &receipt));
    }
    if !(1..=300).contains(&ttl.as_secs()) {
        return Err(failure(ProbeFailureKind::InvalidPolicy, &receipt));
    }
    let root = validate_control_root(control_root).map_err(|diagnostic| ProbeFailure {
        kind: ProbeFailureKind::InvalidControlRoot,
        control_root_diagnostic: Some(diagnostic),
        receipt: receipt.clone(),
    })?;
    // Version preflight is credential-free, networkless, and cannot dispatch.
    preflight_codex_launch(&runtime.launch)
        .await
        .map_err(|_| failure(ProbeFailureKind::RuntimeLaunch, &receipt))?;
    let probe_id = crate::model::id();
    let home = root.join(format!("status-home-{probe_id}"));
    let name = format!("orbit-status-{probe_id}");
    let lease = AuthLease::acquire(runtime)
        .map_err(|_| failure(ProbeFailureKind::CredentialUnavailable, &receipt))?;
    lease
        .stage_status(&home, &name, &probe_id)
        .map_err(|_| failure(ProbeFailureKind::CredentialUnavailable, &receipt))?;
    receipt.isolated_auth_staged = true;
    let read: std::result::Result<Value, ProbeFailureKind> = async {
        let image = resolve_image(&runtime.launch.image)
            .await
            .map_err(|_| ProbeFailureKind::RuntimeLaunch)?;
        let mut command = command_for_runtime(
            runtime,
            &home,
            &name,
            &image,
            "orbit.status_probe",
            &probe_id,
        )
        .map_err(|_| ProbeFailureKind::RuntimeLaunch)?;
        command.stderr(Stdio::null());
        let mut child = command
            .spawn()
            .map_err(|_| ProbeFailureKind::RuntimeLaunch)?;
        receipt.runtime_launched = true;
        let input = child.stdin.take().ok_or(ProbeFailureKind::RuntimeLaunch)?;
        let output = child.stdout.take().ok_or(ProbeFailureKind::RuntimeLaunch)?;
        let mut wire = Wire::new(output, input, MAX_WIRE_BYTES).codex();
        let result = tokio::time::timeout(PROBE_TIMEOUT, protocol(&mut wire, &mut receipt)).await;
        drop(wire);
        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        if child
            .try_wait()
            .map_err(|_| ProbeFailureKind::RuntimeLaunch)?
            .is_none()
        {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        }
        match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) if error.downcast_ref::<StreamClosed>().is_some() => {
                Err(ProbeFailureKind::UnexpectedEof)
            }
            Ok(Err(error)) if error.downcast_ref::<AuthRequired>().is_some() => {
                Err(ProbeFailureKind::Authentication)
            }
            Ok(Err(_)) => Err(ProbeFailureKind::Protocol),
            Err(_) => Err(ProbeFailureKind::Timeout),
        }
    }
    .await;
    // Never clear the marker unless Podman confirmed removal. An uncertain
    // cleanup quarantines this auth store exactly as for coding dispatch.
    crate::container::remove("podman", &name)
        .await
        .map_err(|_| failure(ProbeFailureKind::CleanupUncertain, &receipt))?;
    lease
        .finish(&home)
        .map_err(|_| failure(ProbeFailureKind::CleanupUncertain, &receipt))?;
    receipt.cleanup_confirmed = true;
    let value = read.map_err(|kind| failure(kind, &receipt))?;
    let observed_at_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| failure(ProbeFailureKind::InvalidPolicy, &receipt))?
            .as_millis(),
    )
    .map_err(|_| failure(ProbeFailureKind::InvalidPolicy, &receipt))?;
    let expires_at_ms = observed_at_ms
        + i64::try_from(ttl.as_millis())
            .map_err(|_| failure(ProbeFailureKind::InvalidPolicy, &receipt))?;
    let (snapshot, provider_scope_fingerprint, matched) =
        normalize_observation(resource, binding, &value, observed_at_ms, expires_at_ms)
            .map_err(|_| failure(ProbeFailureKind::Protocol, &receipt))?;
    receipt.account_scope_matched = matched;
    Ok(ProbeOutcome {
        snapshot,
        receipt,
        provider_scope_fingerprint,
    })
}

fn normalize_observation(
    resource: &ExecutionResourceIdentity,
    binding: ProbeBinding<'_>,
    value: &Value,
    observed_at_ms: i64,
    expires_at_ms: i64,
) -> Result<(AvailabilitySnapshot, Option<String>, bool)> {
    let account_id = value["accountId"].as_str().unwrap_or_default();
    let provider_scope_fingerprint = fingerprint(&resource.credential.provider, account_id);
    let expected = match binding {
        ProbeBinding::ExpectedAccountId(expected)
            if provider_scope_fingerprint.is_some() && account_id == expected =>
        {
            Some(account_id)
        }
        ProbeBinding::ConfirmedFingerprint(expected)
            if provider_scope_fingerprint.as_deref() == Some(expected) =>
        {
            Some(account_id)
        }
        ProbeBinding::Enroll => None,
        _ => None,
    };
    let snapshot = codex_rate_limits_snapshot(
        resource,
        expected.unwrap_or_default(),
        &serde_json::to_vec(value)?,
        observed_at_ms,
        expires_at_ms,
    )?;
    Ok((snapshot, provider_scope_fingerprint, expected.is_some()))
}

#[cfg(test)]
mod tests {
    use super::{
        ControlRootFailureReason, ProbeBinding, ProbeReceipt, normalize_observation,
        private_control_tempdir, protocol, validate_control_parent_fields, validate_control_root,
        validate_private_directory,
    };
    use crate::{
        acp_wire::Wire,
        availability::{
            AvailabilityState, CredentialIdentity, ExecutionResourceIdentity, RuntimeIdentity,
            effective_at,
        },
        provider_scope::fingerprint,
    };
    use anyhow::{Result, ensure};
    use serde_json::{Value, json};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    #[test]
    fn catalog_runtime_is_pinned_and_keeps_each_logical_account_distinct() -> Result<()> {
        let make_credential = |id: &str, reference: &str| -> Result<_> {
            let now = 1;
            Ok(crate::credential_registry::Credential {
                id: id.into(),
                provider: "codex".into(),
                reference: reference.into(),
                generation: 1,
                endpoint: None,
                auth_type: crate::codex_credential_enrollment::CODEX_AUTH_TYPE.into(),
                secret_backend: crate::secret_backend::LOCAL_PRIVATE_ID.into(),
                secret_locator: Some(crate::secret_backend::SecretLocator::new(
                    id,
                    1,
                    "33333333-3333-4333-8333-333333333333",
                )?),
                status: crate::credential_registry::CredentialStatus::Enrolled,
                created_at_ms: now,
                updated_at_ms: now,
            })
        };
        let first = make_credential("11111111-1111-4111-8111-111111111111", "codex-main")?;
        let second = make_credential("22222222-2222-4222-8222-222222222222", "codex-work")?;
        first.validate()?;
        second.validate()?;
        let (runtime, first_resource) = super::cataloged_codex_runtime(&first)?;
        let (_, second_resource) = super::cataloged_codex_runtime(&second)?;

        assert_eq!(
            runtime.launch.image,
            crate::codex_credential_enrollment::CODEX_IMAGE
        );
        assert_eq!(runtime.launch.binary_revision, "0.156.0");
        assert_eq!(runtime.auth.files.len(), 1);
        assert_eq!(
            runtime.auth.files.get("auth.json").unwrap(),
            ".codex/auth.json"
        );
        assert_eq!(first_resource.credential.reference, "codex-main");
        assert_eq!(second_resource.credential.reference, "codex-work");
        assert_ne!(
            first_resource.credential.catalog_id,
            second_resource.credential.catalog_id
        );
        assert_ne!(first_resource.id()?, second_resource.id()?);
        Ok(())
    }

    #[test]
    fn default_tempdir_permissions_fail_with_bounded_specific_diagnostic() -> Result<()> {
        let root = tempfile::tempdir()?;
        // tempfile::tempdir defaults to 0o777 & !umask. Set the observed
        // 0o755 mode explicitly so this regression does not depend on umask.
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755))?;
        let metadata = std::fs::symlink_metadata(root.path())?;
        let error = validate_control_root(root.path()).expect_err("broad root must be rejected");
        assert_eq!(error.reason, ControlRootFailureReason::PermissionsTooBroad);
        assert_eq!(error.observed_uid, Some(unsafe { libc::geteuid() }));
        assert_eq!(error.mode, Some(0o755));
        assert!(serde_json::to_string(&error)?.contains("permissions_too_broad"));
        assert!(!serde_json::to_string(&error)?.contains(root.path().to_string_lossy().as_ref()));
        assert_eq!(
            validate_private_directory(&metadata, unsafe { libc::geteuid() }),
            Err(ControlRootFailureReason::PermissionsTooBroad)
        );
        let failure = super::ProbeFailure {
            kind: super::ProbeFailureKind::InvalidControlRoot,
            control_root_diagnostic: Some(error),
            receipt: ProbeReceipt::default(),
        };
        assert!(failure.to_string().contains("PermissionsTooBroad"));
        assert!(failure.to_string().contains("0o755"));
        assert!(
            !failure
                .to_string()
                .contains(root.path().to_string_lossy().as_ref())
        );
        Ok(())
    }

    #[test]
    fn private_temp_child_is_accepted_under_a_safe_shared_parent() -> Result<()> {
        let root = private_control_tempdir()?;
        let metadata = std::fs::symlink_metadata(root.path())?;
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(metadata.mode() & 0o777, 0o700);
        assert_eq!(validate_control_root(root.path()).unwrap(), root.path());

        let parent = std::fs::symlink_metadata(root.path().parent().expect("temp root parent"))?;
        let parent_mode = parent.mode();
        if parent_mode & 0o022 != 0 {
            assert_ne!(
                parent_mode & 0o1000,
                0,
                "shared writable parent must be sticky"
            );
        }
        Ok(())
    }

    #[test]
    fn control_root_missing_and_symlink_cases_have_distinct_reasons() -> Result<()> {
        let parent = private_control_tempdir()?;
        let missing = parent.path().join("missing-control-root");
        assert_eq!(
            validate_control_root(&missing)
                .expect_err("missing root rejected")
                .reason,
            ControlRootFailureReason::NotFound
        );

        let target = private_control_tempdir()?;
        let link = parent.path().join("control-root-link");
        std::os::unix::fs::symlink(target.path(), &link)?;
        assert_eq!(
            validate_control_root(&link)
                .expect_err("symlink root rejected")
                .reason,
            ControlRootFailureReason::SymlinkRejected
        );
        Ok(())
    }

    #[test]
    fn control_root_must_remain_accessible_to_its_owner() -> Result<()> {
        let root = private_control_tempdir()?;
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o000))?;
        let metadata = std::fs::symlink_metadata(root.path())?;
        assert_eq!(
            validate_private_directory(&metadata, unsafe { libc::geteuid() }),
            Err(ControlRootFailureReason::OwnerPermissionsInsufficient)
        );
        Ok(())
    }

    #[test]
    fn control_root_owner_and_shared_parent_policy_fail_closed() -> Result<()> {
        let root = private_control_tempdir()?;
        let metadata = std::fs::symlink_metadata(root.path())?;
        let different_uid = unsafe { libc::geteuid() }.wrapping_add(1);
        assert_eq!(
            validate_private_directory(&metadata, different_uid),
            Err(ControlRootFailureReason::WrongOwner)
        );
        assert_eq!(
            validate_control_parent_fields(1234, 0o755, unsafe { libc::geteuid() }),
            Err(ControlRootFailureReason::ParentInsecure)
        );
        assert_eq!(
            validate_control_parent_fields(1234, 0o1777, unsafe { libc::geteuid() }),
            Ok(())
        );
        assert_eq!(
            validate_control_parent_fields(unsafe { libc::geteuid() }, 0o777, unsafe {
                libc::geteuid()
            }),
            Err(ControlRootFailureReason::ParentInsecure)
        );
        Ok(())
    }

    async fn fixture_reply(server: &mut Wire, method: &str, result: Value) -> Result<()> {
        let request = server.read().await?;
        ensure!(request["method"] == method, "unexpected status method");
        server.response_ok(request["id"].clone(), result).await
    }

    fn resource() -> ExecutionResourceIdentity {
        ExecutionResourceIdentity {
            runtime: RuntimeIdentity {
                family: "acp".into(),
                adapter: "codex_bridge".into(),
                binding: "fixture".into(),
                image_digest: format!("sha256:{}", "a".repeat(64)),
                agent_revision: "0.156.0".into(),
                adapter_version: "1".into(),
            },
            credential: CredentialIdentity {
                provider: "openai".into(),
                reference: "fixture".into(),
                generation: "1".into(),
                catalog_id: None,
            },
            model: "fixture-model".into(),
            reasoning_effort: None,
        }
    }

    #[test]
    fn account_binding_modes_are_exact_and_enrollment_never_ready() -> Result<()> {
        let r = resource();
        let value = json!({"accountId":"personal-scope","ordinaryUsageAllowed":true,
            "rateLimits":{"primary":{"usedPercent":12,"resetsAt":null}}});
        let (strong, scope, matched) = normalize_observation(
            &r,
            ProbeBinding::ExpectedAccountId("personal-scope"),
            &value,
            10,
            20,
        )?;
        assert_eq!(strong.state, AvailabilityState::Ready);
        assert!(matched);
        let scope = scope.expect("valid opaque scope");
        let (wrong, _, matched) = normalize_observation(
            &r,
            ProbeBinding::ExpectedAccountId("other-scope"),
            &value,
            10,
            20,
        )?;
        assert_eq!(wrong.state, AvailabilityState::Unknown);
        assert!(!matched);
        let (first, observed, matched) =
            normalize_observation(&r, ProbeBinding::Enroll, &value, 10, 20)?;
        assert_eq!(observed.as_deref(), Some(scope.as_str()));
        assert_eq!(first.state, AvailabilityState::Unknown);
        assert!(!matched);
        assert_eq!(
            effective_at(&r, std::slice::from_ref(&first), 11)?.state,
            AvailabilityState::Unknown
        );
        let (later, _, matched) = normalize_observation(
            &r,
            ProbeBinding::ConfirmedFingerprint(&scope),
            &value,
            11,
            21,
        )?;
        assert_eq!(later.state, AvailabilityState::Ready);
        assert!(matched);
        assert_eq!(
            effective_at(&r, &[later], 22)?.state,
            AvailabilityState::Unknown
        );
        let (mismatch, _, matched) = normalize_observation(
            &r,
            ProbeBinding::ConfirmedFingerprint(&fingerprint("openai", "other-scope").unwrap()),
            &value,
            11,
            21,
        )?;
        assert_eq!(mismatch.state, AvailabilityState::Unknown);
        assert!(!matched);
        assert!(!serde_json::to_string(&first)?.contains("personal-scope"));
        Ok(())
    }

    #[test]
    fn malformed_scope_never_establishes_enrollment() -> Result<()> {
        let r = resource();
        for account_id in [json!(null), json!(42), json!(""), json!("x".repeat(257))] {
            let value = json!({"accountId":account_id,"ordinaryUsageAllowed":true,"rateLimits":{}});
            let (snapshot, scope, matched) =
                normalize_observation(&r, ProbeBinding::Enroll, &value, 10, 20)?;
            assert!(scope.is_none());
            assert_eq!(snapshot.state, AvailabilityState::Unknown);
            assert!(!matched);
        }
        Ok(())
    }

    #[tokio::test]
    async fn status_protocol_has_no_thread_turn_or_broker_requests() -> Result<()> {
        let (client_read, server_write) = tokio::io::duplex(4096);
        let (server_read, client_write) = tokio::io::duplex(4096);
        let server_task = async move {
            let mut server = Wire::new(server_read, server_write, 65_536).codex();
            fixture_reply(
                &mut server,
                "initialize",
                json!({"codexHome":"/orbit/home/.codex"}),
            )
            .await?;
            ensure!(
                server.read().await?["method"] == "initialized",
                "initialized missing"
            );
            fixture_reply(
                &mut server,
                "account/read",
                json!({"account":{"type":"chatgpt"}}),
            )
            .await?;
            fixture_reply(
                &mut server,
                "account/rateLimits/read",
                json!({"accountId":"expected","rateLimits":{},"ordinaryUsageAllowed":true}),
            )
            .await?;
            // Once status returned, no model/broker protocol message can follow.
            ensure!(
                tokio::time::timeout(std::time::Duration::from_millis(100), server.read())
                    .await
                    .is_err(),
                "unexpected extra request"
            );
            Ok::<_, anyhow::Error>(())
        };
        let mut wire = Wire::new(client_read, client_write, 65_536).codex();
        let mut receipt = ProbeReceipt::default();
        let (server_result, client_result) =
            tokio::join!(server_task, protocol(&mut wire, &mut receipt));
        server_result?;
        let result = client_result?;
        ensure!(result["accountId"] == "expected", "status result missing");
        ensure!(
            receipt.protocol_initialized
                && receipt.authenticated_account_present
                && receipt.status_request_sent
                && receipt.correlated_status_response,
            "status lifecycle incomplete"
        );
        ensure!(
            !receipt.model_thread_created
                && !receipt.model_turn_started
                && receipt.broker_callbacks == 0
                && receipt.broker_filesystem_callbacks == 0
                && receipt.broker_terminal_callbacks == 0
                && receipt.repository_effects == 0,
            "unexpected effects"
        );
        Ok(())
    }

    #[tokio::test]
    async fn status_protocol_rejects_server_callback() -> Result<()> {
        let (client_read, server_write) = tokio::io::duplex(4096);
        let (server_read, client_write) = tokio::io::duplex(4096);
        let server_task = async move {
            let mut server = Wire::new(server_read, server_write, 65_536).codex();
            let _ = server.read().await?;
            server
                .request("item/tool/call", json!({"name":"write"}))
                .await?;
            Ok::<_, anyhow::Error>(())
        };
        let mut wire = Wire::new(client_read, client_write, 65_536).codex();
        let mut receipt = ProbeReceipt::default();
        let (server_result, client_result) =
            tokio::join!(server_task, protocol(&mut wire, &mut receipt));
        server_result?;
        ensure!(client_result.is_err(), "callback accepted");
        ensure!(
            !receipt.status_request_sent && receipt.broker_callbacks == 0,
            "unexpected status state"
        );
        Ok(())
    }

    #[tokio::test]
    async fn status_protocol_rejects_auth_failure_before_status_read() -> Result<()> {
        let (client_read, server_write) = tokio::io::duplex(4096);
        let (server_read, client_write) = tokio::io::duplex(4096);
        let server_task = async move {
            let mut server = Wire::new(server_read, server_write, 65_536).codex();
            fixture_reply(
                &mut server,
                "initialize",
                json!({"codexHome":"/orbit/home/.codex"}),
            )
            .await?;
            ensure!(
                server.read().await?["method"] == "initialized",
                "initialized missing"
            );
            fixture_reply(
                &mut server,
                "account/read",
                json!({"account":null,"requiresOpenaiAuth":true}),
            )
            .await?;
            Ok::<_, anyhow::Error>(())
        };
        let mut wire = Wire::new(client_read, client_write, 65_536).codex();
        let mut receipt = ProbeReceipt::default();
        let (server_result, client_result) =
            tokio::join!(server_task, protocol(&mut wire, &mut receipt));
        server_result?;
        ensure!(
            client_result.is_err() && !receipt.status_request_sent,
            "authentication failure reached status read"
        );
        Ok(())
    }

    #[tokio::test]
    async fn status_protocol_eof_and_timeout_fail_closed() -> Result<()> {
        let mut eof = Wire::new(&b""[..], tokio::io::sink(), 65_536).codex();
        let mut receipt = ProbeReceipt::default();
        ensure!(
            protocol(&mut eof, &mut receipt).await.is_err(),
            "EOF accepted"
        );
        ensure!(
            !receipt.status_request_sent && !receipt.correlated_status_response,
            "EOF fabricated status"
        );
        let (reader, _writer) = tokio::io::duplex(4096);
        let mut blocked = Wire::new(reader, tokio::io::sink(), 65_536).codex();
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(20),
            protocol(&mut blocked, &mut receipt),
        )
        .await;
        ensure!(
            result.is_err() && !receipt.correlated_status_response,
            "timeout fabricated status"
        );
        Ok(())
    }
}
