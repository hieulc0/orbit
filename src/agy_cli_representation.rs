//! Operator-only import and reuse qualification for Antigravity's file-backed
//! agy CLI representation. This does not run provider status or model actions.
use crate::{
    credential_registry::{
        CredentialStatus, CredentialStore, RepresentationState, RuntimeProvenance,
    },
    secret_backend::{LOCAL_PRIVATE_ID, LocalPrivateSecretBackend, SecretBackend, SecretBytes},
};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    task::JoinHandle,
};
use zeroize::{Zeroize, Zeroizing};

pub const AGY_CLI_INTERFACE: &str = "agy-cli";
pub const AGY_CLI_AUTH_TYPE: &str = "oauth-personal";
pub const AGY_CLI_ARTIFACT: &str = "agy-cli";
pub const AGY_CLI_VERSION: &str = "1.2.9";
pub const AGY_CLI_SHA256: &str = "1dbb10f8295cc1ad2e558bd006c7808fe53b6c7f678a887eb557b576bb591711";
const AGY_CLI_SOURCE_RELATIVE: &str = ".local/bin/agy";
const AGY_CLI_PINNED_RELATIVE: &str = ".orbit/private/runtime-artifacts/antigravity-cli/agy-1.2.9-1dbb10f8295cc1ad2e558bd006c7808fe53b6c7f678a887eb557b576bb591711/agy";
const QUALIFIED_TOKEN_SOURCE_RELATIVE: &str =
    ".orbit/private/qualification/agy-login/home/.gemini/antigravity-cli/antigravity-oauth-token";
const TOKEN_RELATIVE: &str = ".gemini/antigravity-cli/antigravity-oauth-token";
const ISOLATED_HOME: &str = "/home/orbit";
const ISOLATED_BINARY: &str = "/run/orbit/agy";
const MAX_CAPTURED_OUTPUT: usize = 64 * 1024;
const STARTUP_OBSERVATION: Duration = Duration::from_secs(20);
const USAGE_TIMEOUT: Duration = Duration::from_secs(60);
const AGY_STATUS_REFERENCE: &str = "antigravity-oauth-test";

const RESOLVE_BENEATH: u64 = 0x08;
const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_NO_SYMLINKS: u64 = 0x04;

fn operator_home() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("operator HOME unavailable")?;
    let home = PathBuf::from(home);
    ensure!(
        home.is_absolute() && home.canonicalize()? == home,
        "operator HOME must be canonical"
    );
    let metadata = fs::symlink_metadata(&home)?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o022 == 0,
        "operator HOME owner or mode invalid"
    );
    Ok(home)
}

fn source_binary_path() -> Result<PathBuf> {
    Ok(operator_home()?.join(AGY_CLI_SOURCE_RELATIVE))
}

fn pinned_binary_path() -> Result<PathBuf> {
    Ok(operator_home()?.join(AGY_CLI_PINNED_RELATIVE))
}

fn qualified_token_source_path() -> Result<PathBuf> {
    Ok(operator_home()?.join(QUALIFIED_TOKEN_SOURCE_RELATIVE))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IsolatedInvocation {
    Version,
    Startup,
    Models,
    Usage,
}

pub struct AgyUsageCaptureReceipt {
    pub response_bytes: usize,
    pub status: crate::provider_status::AntigravityUsageCapture,
    pub token_metadata_changed: bool,
    pub created_or_changed_home_entries: Vec<Value>,
}

struct IsolatedOutput {
    stdout: Zeroizing<Vec<u8>>,
    stderr: Zeroizing<Vec<u8>>,
    success: bool,
    timed_out: bool,
}

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgyCliRepresentationResult {
    pub credential_id: String,
    pub reference: String,
    pub generation: u64,
    pub credential_status: CredentialStatus,
    pub representation_state: RepresentationState,
    pub validation: &'static str,
    pub auth_type: &'static str,
    pub runtime_version: &'static str,
    pub runtime_sha256: &'static str,
    pub runtime_provenance: &'static str,
    pub identity_binding: &'static str,
}

/// Import the operator-qualified token into a PENDING representation, then
/// activate it only after a fresh isolated process accepts the backend copy.
pub async fn import_and_validate(
    pool: &PgPool,
    backend: &LocalPrivateSecretBackend,
    reference: &str,
    source_path: &Path,
) -> Result<AgyCliRepresentationResult> {
    ensure!(
        backend.backend_id() == LOCAL_PRIVATE_ID,
        "unexpected secret backend"
    );
    let qualified_source = qualified_token_source_path()?;
    let source_binary = source_binary_path()?;
    ensure!(
        source_path == qualified_source,
        "only the operator-qualified agy token source may be imported"
    );
    verify_binary_identity(&source_binary).await?;

    let store = CredentialStore::new(pool);
    let credential = store
        .get(reference)
        .await?
        .context("credential not found")?;
    ensure!(
        credential.provider == "antigravity"
            && credential.generation == 1
            && credential.status == CredentialStatus::Enrolled
            && credential.secret_backend == LOCAL_PRIVATE_ID,
        "credential is not the expected enrolled Antigravity generation"
    );
    let inspection = store
        .inspect(reference)
        .await?
        .context("credential inspection unavailable")?;
    let acp = inspection
        .representations
        .iter()
        .find(|representation| representation.interface == "acp" && representation.generation == 1)
        .context("qualified ACP representation missing")?;
    ensure!(
        acp.state == RepresentationState::Stored
            && acp.validation == "valid"
            && acp.auth_type == AGY_CLI_AUTH_TYPE,
        "ACP representation is not in its expected qualified state"
    );
    let provenance = RuntimeProvenance {
        artifact: AGY_CLI_ARTIFACT.to_owned(),
        version: AGY_CLI_VERSION.to_owned(),
        sha256: AGY_CLI_SHA256.to_owned(),
        provenance: "operator-supplied".to_owned(),
    };
    let existing = inspection.representations.iter().find(|representation| {
        representation.interface == AGY_CLI_INTERFACE && representation.generation == 1
    });
    let prepared = if let Some(view) = existing {
        let stored = store
            .representation(&view.id)
            .await?
            .context("existing agy-cli representation disappeared")?;
        ensure!(
            matches!(
                stored.state,
                RepresentationState::Pending | RepresentationState::Stored
            ) && stored.auth_type == AGY_CLI_AUTH_TYPE
                && stored.runtime_provenance.as_ref() == Some(&provenance),
            "existing agy-cli representation is not safely resumable"
        );
        let locator = stored
            .secret_locator
            .context("existing agy-cli representation has no locator")?;
        if backend.exists(locator).await? {
            stored
        } else {
            ensure!(
                stored.state == RepresentationState::Pending,
                "stored agy-cli secret is unavailable"
            );
            let source_secret = read_qualified_token(source_path)?;
            backend.create(locator, source_secret).await?;
            stored
        }
    } else {
        let source_secret = read_qualified_token(source_path)?;
        let pending = store
            .prepare_representation_with_metadata(
                reference,
                AGY_CLI_INTERFACE,
                AGY_CLI_AUTH_TYPE,
                &[],
                backend.backend_id(),
                Some(&provenance),
            )
            .await?;
        let locator = pending
            .secret_locator
            .context("pending agy-cli representation has no locator")?;
        // A failure below intentionally leaves the pending DB row and any
        // published secret as an identifiable, inert orphan candidate.
        backend.create(locator, source_secret).await?;
        pending
    };
    let locator = prepared
        .secret_locator
        .context("agy-cli representation has no locator")?;
    let stored_secret = backend.read(locator).await?;
    let (root, fresh_home) = new_private_home()?;
    stage_only_token(&fresh_home, &stored_secret)?;
    ensure!(
        home_contains_only_candidate(&fresh_home)?,
        "unexpected files in the staged agy-cli HOME"
    );

    // agy 1.2.9 has no auth-status command. `models` is its narrowest
    // non-inference account operation: it validates authentication and exits
    // without creating a conversation, running a prompt, or reading quota.
    let startup = run_isolated(
        &root,
        &fresh_home,
        IsolatedInvocation::Models,
        true,
        &source_binary,
    )
    .await?;
    ensure!(startup.success, "fresh agy authentication reuse failed");
    let observation = observe_startup(&startup.stdout, &startup.stderr);
    ensure!(
        !observation.login_prompt && !observation.network_error && observation.stdout_nonempty,
        "fresh agy process did not demonstrate local authentication reuse (login_prompt={}, network_error={}, stdout_bytes={}, stderr_bytes={})",
        observation.login_prompt,
        observation.network_error,
        startup.stdout.len(),
        startup.stderr.len()
    );
    drop(startup);

    // Check again after execution so a mutated operator-supplied executable is
    // never recorded as the artifact that passed the reuse test.
    verify_binary_identity(&source_binary).await?;

    let finalized = store
        .finalize_validated_representation(backend, &prepared.id)
        .await?;
    ensure!(
        finalized.state == RepresentationState::Stored
            && finalized.last_validated_at_ms.is_some()
            && finalized.auth_type == AGY_CLI_AUTH_TYPE,
        "agy-cli representation finalization did not record validation"
    );
    let binding = store
        .record_operator_intended_identity_binding(reference, "acp", AGY_CLI_INTERFACE)
        .await?;
    ensure!(
        binding.state == "unverified" && binding.basis == "operator-intent",
        "unexpected ACP/ag y identity binding state"
    );
    let after = store
        .get(reference)
        .await?
        .context("credential disappeared")?;
    ensure!(
        after.id == credential.id
            && after.generation == credential.generation
            && after.status == CredentialStatus::Enrolled,
        "credential identity or lifecycle changed unexpectedly"
    );
    let after_inspection = store
        .inspect(reference)
        .await?
        .context("credential inspection unavailable")?;
    let acp_after = after_inspection
        .representations
        .iter()
        .find(|representation| representation.interface == "acp")
        .context("ACP representation disappeared")?;
    ensure!(
        acp_after == acp,
        "ACP representation changed during agy-cli qualification"
    );

    Ok(AgyCliRepresentationResult {
        credential_id: after.id,
        reference: after.reference,
        generation: after.generation,
        credential_status: after.status,
        representation_state: finalized.state,
        validation: "valid",
        auth_type: AGY_CLI_AUTH_TYPE,
        runtime_version: AGY_CLI_VERSION,
        runtime_sha256: AGY_CLI_SHA256,
        runtime_provenance: "operator-supplied",
        identity_binding: "unverified",
    })
}

/// Perform the single authorized status operation from the current registered
/// agy-cli representation. Provider JSON is parsed in memory, converted to
/// bounded value-free schema diagnostics, and discarded without raw retention.
pub async fn capture_registered_usage_once(
    pool: &PgPool,
    backend: &LocalPrivateSecretBackend,
    reference: &str,
) -> Result<AgyUsageCaptureReceipt> {
    ensure!(
        reference == AGY_STATUS_REFERENCE,
        "status qualification is restricted to the authorized credential"
    );
    ensure!(
        backend.backend_id() == LOCAL_PRIVATE_ID,
        "unexpected credential secret backend"
    );
    let pinned_binary = pinned_binary_path()?;
    verify_pinned_binary_identity(&pinned_binary).await?;
    let store = CredentialStore::new(pool);
    let credential = store
        .get(reference)
        .await?
        .context("registered credential unavailable")?;
    ensure!(
        credential.provider == "antigravity"
            && credential.reference == AGY_STATUS_REFERENCE
            && credential.generation == 1
            && credential.status == CredentialStatus::Enrolled
            && credential.secret_backend == LOCAL_PRIVATE_ID,
        "registered Antigravity credential is not in the authorized state"
    );
    let inspection = store
        .inspect(reference)
        .await?
        .context("credential inspection unavailable")?;
    let agy_view = inspection
        .representations
        .iter()
        .find(|representation| {
            representation.interface == AGY_CLI_INTERFACE
                && representation.generation == 1
                && representation.current_generation
        })
        .context("registered agy-cli representation unavailable")?;
    ensure!(
        agy_view.state == RepresentationState::Stored
            && agy_view.validation == "valid"
            && agy_view.auth_type == AGY_CLI_AUTH_TYPE,
        "registered agy-cli representation is not valid"
    );
    let representation = store
        .representation(&agy_view.id)
        .await?
        .context("registered agy-cli representation disappeared")?;
    let provenance = representation
        .runtime_provenance
        .as_ref()
        .context("agy-cli runtime provenance unavailable")?;
    ensure!(
        representation.generation == credential.generation
            && representation.credential_id == credential.id
            && representation.interface == AGY_CLI_INTERFACE
            && representation.auth_type == AGY_CLI_AUTH_TYPE
            && representation.state == RepresentationState::Stored
            && representation.last_validated_at_ms.is_some()
            && provenance.artifact == AGY_CLI_ARTIFACT
            && provenance.version == AGY_CLI_VERSION
            && provenance.sha256 == AGY_CLI_SHA256
            && provenance.provenance == "operator-supplied",
        "registered agy-cli representation does not match the qualified artifact"
    );
    let locator = representation
        .secret_locator
        .context("registered agy-cli secret locator unavailable")?;
    let secret = backend.read(locator).await?;
    let (root, fresh_home) = new_private_home()?;
    stage_only_token(&fresh_home, &secret)?;
    ensure!(
        home_contains_only_candidate(&fresh_home)?,
        "fresh agy status HOME contains unexpected staged files"
    );
    let token_path = fresh_home.join(TOKEN_RELATIVE);
    let token_before = fs::symlink_metadata(&token_path)?;

    // Exactly one provider status command is launched here. No retry or model
    // operation is part of this function.
    let invocation = run_isolated(
        &root,
        &fresh_home,
        IsolatedInvocation::Usage,
        true,
        &pinned_binary,
    )
    .await;
    verify_pinned_binary_identity(&pinned_binary).await?;
    let output = invocation?;
    ensure!(
        !output.timed_out,
        "Antigravity agy status transport timed out"
    );
    if !output.success {
        let mut diagnostic = Vec::with_capacity(output.stdout.len() + output.stderr.len());
        diagnostic.extend_from_slice(&output.stdout);
        diagnostic.extend_from_slice(&output.stderr);
        let class = classify_usage_failure(&diagnostic);
        diagnostic.zeroize();
        anyhow::bail!(class);
    }
    ensure!(
        output.stdout.len() <= MAX_CAPTURED_OUTPUT,
        "Antigravity status response exceeded its bound"
    );
    ensure!(
        output.stdout.len() <= crate::agy_usage_schema::MAX_USAGE_JSON_BYTES,
        "Antigravity status JSON exceeded its input bound"
    );
    let IsolatedOutput {
        stdout: raw,
        stderr,
        ..
    } = output;
    drop(stderr);
    let mut value: Value = serde_json::from_slice(&raw)
        .map_err(|_| anyhow::anyhow!("Antigravity status response was not structured JSON"))?;
    if contains_auth_interaction(&value) {
        crate::agy_usage_schema::scrub_json_values(&mut value);
        anyhow::bail!("Antigravity agy status authentication failed");
    }
    crate::agy_usage_schema::scrub_json_values(&mut value);
    drop(value);
    let status = crate::provider_status::antigravity_usage_capture(&raw)?;
    let token_after = fs::symlink_metadata(&token_path)?;
    let token_metadata_changed = token_before.len() != token_after.len()
        || token_before.mtime() != token_after.mtime()
        || token_before.mtime_nsec() != token_after.mtime_nsec();
    let created_or_changed_home_entries = structural_home_entries(&fresh_home)?;
    let response_bytes = raw.len();
    drop(secret);
    drop(root);

    Ok(AgyUsageCaptureReceipt {
        response_bytes,
        status,
        token_metadata_changed,
        created_or_changed_home_entries,
    })
}

fn contains_auth_interaction(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(value) => {
            let value = Zeroizing::new(value.to_ascii_lowercase());
            [
                "accounts.google.com",
                "authorization required",
                "authentication required",
                "sign in again",
                "please log in",
                "please sign in",
                "unauthorized",
                "unauthenticated",
            ]
            .iter()
            .any(|needle| value.contains(needle))
        }
        serde_json::Value::Object(object) => object.values().any(contains_auth_interaction),
        serde_json::Value::Array(items) => items.iter().any(contains_auth_interaction),
        _ => false,
    }
}

fn classify_usage_failure(output: &[u8]) -> &'static str {
    let mut text = Zeroizing::new(String::from_utf8_lossy(output).to_ascii_lowercase());
    if [
        "accounts.google.com",
        "authorization required",
        "authentication required",
        "sign in again",
        "please log in",
        "please sign in",
        "unauthorized",
        "unauthenticated",
    ]
    .iter()
    .any(|needle| text.contains(needle))
    {
        text.zeroize();
        "Antigravity agy status authentication failed"
    } else if [
        "network is unreachable",
        "connection refused",
        "could not resolve",
        "dns failure",
        "failed to connect",
        "timed out",
    ]
    .iter()
    .any(|needle| text.contains(needle))
    {
        text.zeroize();
        "Antigravity agy status transport failed"
    } else {
        text.zeroize();
        "Antigravity agy status command failed"
    }
}

fn structural_home_entries(home: &Path) -> Result<Vec<Value>> {
    fn visit(root: &Path, path: &Path, entries: &mut Vec<Value>, depth: usize) -> Result<()> {
        ensure!(
            entries.len() < 128 && depth <= 8,
            "agy status HOME entry bound exceeded"
        );
        for item in fs::read_dir(path)? {
            let item = item?;
            let child = item.path();
            let metadata = fs::symlink_metadata(&child)?;
            let kind = if metadata.file_type().is_symlink() {
                "symlink"
            } else if metadata.is_dir() {
                "directory"
            } else if metadata.is_file() {
                "file"
            } else {
                "other"
            };
            let relative = child
                .strip_prefix(root)
                .context("agy status HOME path escaped its root")?;
            entries.push(serde_json::json!({
                "path": relative.to_string_lossy(),
                "type": kind,
                "mode": format!("{:04o}", metadata.mode() & 0o7777),
                "size": metadata.len(),
            }));
            if metadata.is_dir() {
                visit(root, &child, entries, depth + 1)?;
            }
        }
        Ok(())
    }
    let mut entries = Vec::new();
    visit(home, home, &mut entries, 0)?;
    entries.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    Ok(entries)
}

fn new_private_home() -> Result<(TempDir, PathBuf)> {
    let mut builder = tempfile::Builder::new();
    builder
        .prefix("orbit-agy-reuse-")
        .permissions(fs::Permissions::from_mode(0o700));
    let root = builder.tempdir()?;
    let home = root.path().join("home");
    fs::DirBuilder::new().mode(0o700).create(&home)?;
    validate_private_directory(root.path())?;
    validate_private_directory(&home)?;
    Ok((root, home))
}

fn validate_private_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o7777 == 0o700,
        "private agy validation directory owner or mode invalid (uid={}, expected={}, mode={:#o}, directory={})",
        metadata.uid(),
        unsafe { libc::geteuid() },
        metadata.mode() & 0o7777,
        metadata.is_dir()
    );
    Ok(())
}

fn read_qualified_token(path: &Path) -> Result<SecretBytes> {
    validate_source_parent_chain(path)?;
    let mut file = open_beneath(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.nlink() == 1
            && metadata.mode() & 0o7777 == 0o600
            && metadata.len() > 0
            && metadata.len() <= 1024 * 1024,
        "qualified agy token file owner, mode or size invalid"
    );
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    SecretBytes::new(bytes)
}

fn validate_source_parent_chain(path: &Path) -> Result<()> {
    ensure!(
        path == qualified_token_source_path()?,
        "only the operator-qualified agy token source may be imported"
    );
    let orbit_root = operator_home()?.join(".orbit");
    let private_root = orbit_root.join("private");
    let qualification_home = private_root.join("qualification/agy-login/home");
    let mut directory = path
        .parent()
        .context("qualified agy token parent missing")?;
    while directory != qualification_home.as_path() {
        ensure!(
            directory.starts_with(&qualification_home),
            "qualified agy token escaped its private HOME"
        );
        let metadata = fs::symlink_metadata(directory)?;
        ensure!(
            metadata.is_dir()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o022 == 0,
            "qualified agy token parent is unsafe"
        );
        directory = directory
            .parent()
            .context("qualified agy token HOME missing")?;
    }
    validate_private_directory(&qualification_home)?;
    let mut directory = qualification_home
        .parent()
        .context("qualified agy qualification root missing")?;
    loop {
        validate_private_directory(directory)?;
        if directory == private_root.as_path() {
            break;
        }
        directory = directory
            .parent()
            .context("qualified agy token escaped Orbit private root")?;
        ensure!(
            directory.starts_with(&private_root) || directory == orbit_root.as_path(),
            "qualified agy token escaped Orbit private root"
        );
    }
    Ok(())
}

fn open_beneath(path: &Path) -> Result<File> {
    ensure!(
        path.is_absolute() && path.canonicalize()? == path,
        "qualified agy artifact path is not canonical"
    );
    let mut relative = String::new();
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(value) => {
                if !relative.is_empty() {
                    relative.push('/');
                }
                relative.push_str(
                    value
                        .to_str()
                        .context("qualified agy artifact path is not UTF-8")?,
                );
            }
            _ => anyhow::bail!("qualified agy artifact path is not canonical"),
        }
    }
    let root = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open("/")?;
    let relative = CString::new(relative)?;
    let how = OpenHow {
        flags: (libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
        mode: 0,
        resolve: RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS,
    };
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            root.as_raw_fd(),
            relative.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    } as i32;
    ensure!(fd >= 0, "qualified agy artifact unavailable or unsafe");
    Ok(unsafe { File::from_raw_fd(fd) })
}

async fn verify_binary_identity(path: &Path) -> Result<()> {
    let mut file = open_beneath(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o022 == 0
            && metadata.len() > 0
            && metadata.len() <= 256 * 1024 * 1024,
        "operator-supplied agy executable owner, mode or size invalid"
    );
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    buffer.zeroize();
    ensure!(
        hex::encode(digest.finalize()) == AGY_CLI_SHA256,
        "operator-supplied agy executable hash mismatch"
    );

    let (root, home) = new_private_home()?;
    let output = run_isolated(&root, &home, IsolatedInvocation::Version, false, path).await?;
    ensure!(
        output.success,
        "operator-supplied agy version command failed"
    );
    let version = Zeroizing::new(String::from_utf8_lossy(&output.stdout).into_owned());
    let version_ok = version.trim() == AGY_CLI_VERSION;
    ensure!(version_ok, "operator-supplied agy version mismatch");
    Ok(())
}

async fn verify_pinned_binary_identity(path: &Path) -> Result<()> {
    let mut file = open_beneath(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.nlink() == 1
            && metadata.mode() & 0o7777 == 0o500
            && metadata.len() > 0
            && metadata.len() <= 256 * 1024 * 1024,
        "pinned agy executable owner, mode or size invalid"
    );
    let orbit_root = operator_home()?.join(".orbit");
    let private_root = orbit_root.join("private");
    let mut directory = path.parent().context("pinned agy parent unavailable")?;
    loop {
        validate_private_directory(directory)?;
        if directory == private_root.as_path() {
            break;
        }
        directory = directory
            .parent()
            .context("pinned agy artifact escaped Orbit private root")?;
        ensure!(
            directory.starts_with(&private_root) || directory == orbit_root.as_path(),
            "pinned agy artifact escaped Orbit private root"
        );
    }
    validate_private_directory(&orbit_root)?;

    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    buffer.zeroize();
    ensure!(
        hex::encode(digest.finalize()) == AGY_CLI_SHA256,
        "pinned agy executable hash mismatch"
    );
    let (root, home) = new_private_home()?;
    let output = run_isolated(&root, &home, IsolatedInvocation::Version, false, path).await?;
    ensure!(output.success, "pinned agy version command failed");
    let version = Zeroizing::new(String::from_utf8_lossy(&output.stdout).into_owned());
    ensure!(
        version.trim() == AGY_CLI_VERSION,
        "pinned agy version mismatch"
    );
    Ok(())
}

fn stage_only_token(home: &Path, secret: &SecretBytes) -> Result<()> {
    let gemini = home.join(".gemini");
    fs::DirBuilder::new().mode(0o700).create(&gemini)?;
    let cli = gemini.join("antigravity-cli");
    fs::DirBuilder::new().mode(0o700).create(&cli)?;
    let token = cli.join("antigravity-oauth-token");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(token)?;
    file.write_all(secret.expose())?;
    file.sync_all()?;
    let parent = File::open(&cli)?;
    parent.sync_all()?;
    validate_private_directory(&gemini)?;
    validate_private_directory(&cli)?;
    let metadata = fs::symlink_metadata(home.join(TOKEN_RELATIVE))?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.nlink() == 1
            && metadata.mode() & 0o7777 == 0o600,
        "staged agy token owner or mode invalid"
    );
    Ok(())
}

fn home_contains_only_candidate(home: &Path) -> Result<bool> {
    let entries: Vec<_> = fs::read_dir(home)?.collect::<std::io::Result<Vec<_>>>()?;
    if entries.len() != 1 || entries[0].file_name() != ".gemini" {
        return Ok(false);
    }
    let gemini = entries[0].path();
    let children: Vec<_> = fs::read_dir(&gemini)?.collect::<std::io::Result<Vec<_>>>()?;
    if children.len() != 1 || children[0].file_name() != "antigravity-cli" {
        return Ok(false);
    }
    let cli = children[0].path();
    let files: Vec<_> = fs::read_dir(&cli)?.collect::<std::io::Result<Vec<_>>>()?;
    Ok(files.len() == 1 && files[0].file_name() == "antigravity-oauth-token")
}

async fn run_isolated(
    root: &TempDir,
    home: &Path,
    invocation: IsolatedInvocation,
    allow_auth_network: bool,
    binary_path: &Path,
) -> Result<IsolatedOutput> {
    let interactive = invocation == IsolatedInvocation::Startup;
    let mut command = Command::new("bwrap");
    command.args([
        "--die-with-parent",
        "--new-session",
        "--unshare-user",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
    ]);
    if !allow_auth_network {
        command.arg("--unshare-net");
    }
    command.args([
        "--ro-bind",
        "/",
        "/",
        "--tmpfs",
        "/run",
        "--dir",
        "/run/orbit",
        "--ro-bind",
    ]);
    command.arg(binary_path);
    command.args([
        ISOLATED_BINARY,
        "--tmpfs",
        "/home",
        "--dir",
        ISOLATED_HOME,
        "--bind",
    ]);
    command.arg(home);
    command.args([
        ISOLATED_HOME,
        "--tmpfs",
        "/tmp",
        "--tmpfs",
        "/var/tmp",
        "--dev",
        "/dev",
        "--proc",
        "/proc",
        "--clearenv",
        "--setenv",
        "HOME",
        ISOLATED_HOME,
        "--setenv",
        "PATH",
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        "--setenv",
        "AGY_CLI_DISABLE_AUTO_UPDATE",
        "true",
        "--setenv",
        "TERM",
        "xterm-256color",
    ]);
    if invocation != IsolatedInvocation::Usage {
        command.args([
            "--setenv",
            "SSH_CLIENT",
            "127.0.0.1 12345 22",
            "--setenv",
            "SSH_CONNECTION",
            "127.0.0.1 12345 127.0.0.1 22",
        ]);
    }
    command.arg("--");
    match invocation {
        IsolatedInvocation::Startup => {
            command.args(["/usr/bin/script", "-qefc", ISOLATED_BINARY, "/dev/null"]);
        }
        IsolatedInvocation::Version => {
            command.args([ISOLATED_BINARY, "--version"]);
        }
        IsolatedInvocation::Models => {
            command.args([ISOLATED_BINARY, "models"]);
        }
        IsolatedInvocation::Usage => {
            command.args([
                ISOLATED_BINARY,
                "--print",
                "/usage",
                "--output-format",
                "json",
            ]);
        }
    }
    command
        .stdin(if interactive {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .context("isolated agy runtime could not start")?;
    let interactive_stdin = if interactive {
        Some(
            child
                .stdin
                .take()
                .context("isolated agy stdin unavailable")?,
        )
    } else {
        None
    };
    let stdout = child
        .stdout
        .take()
        .context("isolated agy stdout unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("isolated agy stderr unavailable")?;
    // `script` allocates the PTY but does not emulate a terminal. Answer only
    // agy's device-attributes query after it is observed; no keys, commands,
    // or auth codes are sent during this startup-only observation.
    let stdout_task = if let Some(stdin) = interactive_stdin {
        tokio::spawn(read_capped_with_terminal_response(stdout, stdin))
    } else {
        tokio::spawn(read_capped(stdout))
    };
    let stderr_task = tokio::spawn(read_capped(stderr));
    let (success, timed_out) = wait_or_stop(&mut child, invocation).await?;
    let stdout = collect_output(stdout_task).await?;
    let stderr = collect_output(stderr_task).await?;
    let _ = root;
    Ok(IsolatedOutput {
        stdout,
        stderr,
        success,
        timed_out,
    })
}

async fn read_capped<R>(reader: R) -> Result<Zeroizing<Vec<u8>>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut output = Zeroizing::new(Vec::new());
    reader
        .take((MAX_CAPTURED_OUTPUT + 1) as u64)
        .read_to_end(&mut output)
        .await?;
    ensure!(
        output.len() <= MAX_CAPTURED_OUTPUT,
        "isolated agy output exceeded its bound"
    );
    Ok(output)
}

async fn read_capped_with_terminal_response<R, W>(
    mut reader: R,
    mut writer: W,
) -> Result<Zeroizing<Vec<u8>>>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut output = Zeroizing::new(Vec::new());
    let mut buffer = [0_u8; 4096];
    let mut primary_answered = false;
    let mut secondary_answered = false;
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        ensure!(
            output.len() + count <= MAX_CAPTURED_OUTPUT,
            "isolated agy output exceeded its bound"
        );
        output.extend_from_slice(&buffer[..count]);
        if !primary_answered && output.windows(3).any(|window| window == b"\x1b[c") {
            writer.write_all(b"\x1b[?1;2c").await?;
            writer.flush().await?;
            primary_answered = true;
        }
        if !secondary_answered && output.windows(4).any(|window| window == b"\x1b[>c") {
            writer.write_all(b"\x1b[>0;0;0c").await?;
            writer.flush().await?;
            secondary_answered = true;
        }
    }
    buffer.zeroize();
    Ok(output)
}

async fn collect_output(
    task: JoinHandle<Result<Zeroizing<Vec<u8>>>>,
) -> Result<Zeroizing<Vec<u8>>> {
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .context("isolated agy output did not close")?
        .context("isolated agy output task failed")?
}

async fn wait_or_stop(child: &mut Child, invocation: IsolatedInvocation) -> Result<(bool, bool)> {
    if invocation == IsolatedInvocation::Usage {
        return match tokio::time::timeout(USAGE_TIMEOUT, child.wait()).await {
            Ok(status) => Ok((status?.success(), false)),
            Err(_) => {
                child
                    .kill()
                    .await
                    .context("timed-out agy status process could not be stopped")?;
                let _ = child.wait().await;
                Ok((false, true))
            }
        };
    }
    if invocation != IsolatedInvocation::Startup {
        let timeout = if invocation == IsolatedInvocation::Models {
            STARTUP_OBSERVATION
        } else {
            Duration::from_secs(5)
        };
        let status = tokio::time::timeout(timeout, child.wait())
            .await
            .context("isolated agy validation command timed out")??;
        return Ok((status.success(), false));
    }
    tokio::select! {
        status = child.wait() => {
            let status = status?;
            ensure!(status.success(), "isolated agy process exited unsuccessfully");
        }
        _ = tokio::time::sleep(STARTUP_OBSERVATION) => {
            child.kill().await.context("isolated agy process could not be stopped")?;
            let _ = child.wait().await;
        }
    }
    Ok((true, false))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StartupObservation {
    sign_in_progress: bool,
    welcome_marker: bool,
    main_ui_marker: bool,
    login_prompt: bool,
    network_error: bool,
    cursor_query: bool,
    primary_device_query: bool,
    secondary_device_query: bool,
    color_query: bool,
    stdout_nonempty: bool,
    stderr_nonempty: bool,
}

#[cfg(test)]
impl StartupObservation {
    fn reused(self) -> bool {
        ((self.sign_in_progress && self.welcome_marker) || self.main_ui_marker)
            && !self.login_prompt
            && !self.network_error
    }
}

fn observe_startup(stdout: &[u8], stderr: &[u8]) -> StartupObservation {
    let mut combined = Zeroizing::new(Vec::with_capacity(stdout.len() + stderr.len()));
    combined.extend_from_slice(stdout);
    combined.extend_from_slice(stderr);
    let plain = strip_terminal_control(&combined);
    let text = Zeroizing::new(String::from_utf8_lossy(&plain).to_ascii_lowercase());
    StartupObservation {
        sign_in_progress: text.contains("signing in"),
        welcome_marker: text.contains("welcome to antigravity cli")
            || text.contains("welcome to the antigravity cli"),
        // agy's full-screen TUI clears its transient "Signing in" and welcome
        // text. These bounded, non-account labels are stable main-screen chrome
        // and therefore positive evidence that startup advanced past login.
        main_ui_marker: [
            "[no workspace]",
            "workspace view",
            "type to search conversations",
            "view background tasks",
            "before tool execution",
        ]
        .iter()
        .any(|marker| text.contains(marker)),
        network_error: [
            "network is unreachable",
            "connection refused",
            "could not resolve",
            "dns failure",
            "failed to connect",
        ]
        .iter()
        .any(|marker| text.contains(marker)),
        cursor_query: combined.windows(4).any(|window| window == b"\x1b[6n"),
        primary_device_query: combined.windows(3).any(|window| window == b"\x1b[c"),
        secondary_device_query: combined.windows(4).any(|window| window == b"\x1b[>c"),
        color_query: combined.windows(6).any(|window| window == b"\x1b]11;?")
            || combined.windows(6).any(|window| window == b"\x1b]10;?"),
        stdout_nonempty: !stdout.is_empty(),
        stderr_nonempty: !stderr.is_empty(),
        login_prompt: [
            "accounts.google.com",
            "open this url",
            "authorization code",
            "paste the code",
            "log in with google",
            "sign in with google",
            "sign in to continue",
            "not signed in",
            "authentication required",
            "log in, then retry",
            "select an account",
            "choose an account",
            "choose an authentication method",
            "google personal",
        ]
        .iter()
        .any(|marker| text.contains(marker)),
    }
}

fn strip_terminal_control(bytes: &[u8]) -> Zeroizing<Vec<u8>> {
    let mut plain = Zeroizing::new(Vec::with_capacity(bytes.len()));
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            0x1b => {
                index += 1;
                if index >= bytes.len() {
                    break;
                }
                match bytes[index] {
                    b'[' => {
                        index += 1;
                        while index < bytes.len() {
                            let byte = bytes[index];
                            index += 1;
                            if (0x40..=0x7e).contains(&byte) {
                                break;
                            }
                        }
                    }
                    b']' | b'P' | b'_' | b'^' => {
                        let osc = bytes[index] == b']';
                        index += 1;
                        while index < bytes.len() {
                            if osc && bytes[index] == 0x07 {
                                index += 1;
                                break;
                            }
                            if bytes[index] == 0x1b && bytes.get(index + 1).copied() == Some(b'\\')
                            {
                                index += 2;
                                break;
                            }
                            index += 1;
                        }
                    }
                    _ => index += 1,
                }
            }
            b'\r' => {
                plain.push(b' ');
                index += 1;
            }
            byte if byte < 0x20 && !matches!(byte, b'\n' | b'\t') => index += 1,
            byte => {
                plain.push(byte);
                index += 1;
            }
        }
    }
    plain
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn only_the_qualified_startup_state_counts_as_reuse() {
        assert!(observe_startup(b"Signing in...\r\nWelcome to Antigravity CLI!", b"").reused());
        assert!(observe_startup(b"[No Workspace]\r\nWorkspace View", b"").reused());
        assert!(
            !observe_startup(
                b"Signing in...\r\nOpen this URL https://accounts.google.com/oauth",
                b""
            )
            .reused()
        );
        assert!(!observe_startup(b"Welcome to Antigravity CLI!", b"").reused());
        assert!(
            !observe_startup(
                b"Welcome to the Antigravity CLI. You are currently not signed in.",
                b""
            )
            .reused()
        );
        assert!(
            !observe_startup(
                b"Error: authentication required. Run 'agy auth login' to log in, then retry.",
                b""
            )
            .reused()
        );
        let styled = b"\x1b[32mSigning in...\x1b[0m\n\x1b[1mWelcome to Antigravity CLI!\x1b[0m";
        assert!(observe_startup(styled, b"").reused());
    }

    #[test]
    fn staging_contains_only_one_private_token_file() -> Result<()> {
        let root = crate::codex_status_probe::private_control_tempdir()?;
        let home = root.path().join("home");
        fs::DirBuilder::new().mode(0o700).create(&home)?;
        let secret = SecretBytes::new(b"qualified-test-token".to_vec())?;
        stage_only_token(&home, &secret)?;
        assert!(home_contains_only_candidate(&home)?);
        let token = fs::symlink_metadata(home.join(TOKEN_RELATIVE))?;
        assert_eq!(token.mode() & 0o7777, 0o600);
        Ok(())
    }

    #[test]
    fn fresh_validation_home_is_private() -> Result<()> {
        let (_root, home) = new_private_home()?;
        validate_private_directory(&home)
    }

    #[test]
    fn qualified_source_symlinks_are_rejected_without_reading_content() -> Result<()> {
        let root = crate::codex_status_probe::private_control_tempdir()?;
        let real = root.path().join("real");
        fs::write(&real, b"not-read")?;
        fs::set_permissions(&real, fs::Permissions::from_mode(0o600))?;
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&real, &link)?;
        assert!(open_beneath(&link).is_err());
        Ok(())
    }
}
