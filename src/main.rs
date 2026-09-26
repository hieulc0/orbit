use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use orbit::{
    api::{self, App, Config, Submit},
    availability::AvailabilityStore,
    credential_registry::CredentialStore,
    engine::Engine,
    model::{Definition, Limits, Signal, id},
    secret_backend::SecretBackend,
    worker::{self, Client},
};
use sqlx::Row;
use std::{
    fs::{self, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use zeroize::Zeroizing;

#[derive(Parser)]
#[command(
    version,
    about = "Durable repository work, from request to tested patch"
)]
struct Cli {
    #[arg(long, global = true, value_enum, default_value = "json")]
    output_format: Output,
    #[arg(long, env = "ORBIT_URL", default_value = "http://127.0.0.1:7700")]
    url: String,
    #[arg(long, env = "ORBIT_TOKEN", hide_env_values = true, default_value = "")]
    token: String,
    #[arg(long, global = true, env = "ORBIT_TOKEN_FILE", hide_env_values = true)]
    token_file: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Output {
    Json,
    Jsonl,
    Text,
}
fn scope_query(scope: Option<&str>) -> Result<String> {
    if let Some(value) = scope {
        orbit::governance::Scope::parse(value)?;
        Ok(format!("?scope={value}"))
    } else {
        Ok(String::new())
    }
}
#[derive(clap::Args, Clone, Debug)]
struct RunArgs {
    #[command(subcommand)]
    action: Option<RunAction>,
    /// Path to definition YAML file (legacy positional syntax: orbit run <DEFINITION>)
    #[arg(value_name = "DEFINITION")]
    definition: Option<PathBuf>,
    /// Base Git revision override (e.g. HEAD, branch name, or commit SHA)
    #[arg(long)]
    base_revision: Option<String>,
    /// Task description override
    #[arg(long)]
    task: Option<String>,
    #[arg(long)]
    scope: Option<String>,
    #[arg(long)]
    request_id: Option<String>,
    #[arg(long)]
    parent_run_id: Option<String>,
}

#[derive(Subcommand, Clone, Debug)]
enum RunAction {
    /// Submit a workflow run
    Submit(RunSubmitArgs),
}

#[derive(clap::Args, Clone, Debug)]
struct RunSubmitArgs {
    /// Path to definition YAML file
    #[arg(long)]
    definition: PathBuf,
    /// Base Git revision override (e.g. HEAD, branch name, or commit SHA)
    #[arg(long)]
    base_revision: Option<String>,
    /// Task description override
    #[arg(long)]
    task: Option<String>,
    #[arg(long)]
    scope: Option<String>,
    #[arg(long)]
    request_id: Option<String>,
    #[arg(long)]
    parent_run_id: Option<String>,
}

#[derive(clap::Args)]
struct CredentialArgs {
    #[command(subcommand)]
    action: CredentialAction,
}

#[derive(Subcommand)]
enum CredentialAction {
    /// Enroll an operator-owned provider credential through its native login flow.
    Add {
        provider: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        auth_method: Option<String>,
        #[arg(long, env = "ORBIT_DATABASE_URL_FILE", hide_env_values = true)]
        database_url_file: Option<PathBuf>,
    },
    /// Rename a logical credential reference without changing its identity or evidence.
    Rename {
        old_reference: String,
        new_reference: String,
        #[arg(long, env = "ORBIT_DATABASE_URL_FILE", hide_env_values = true)]
        database_url_file: Option<PathBuf>,
    },
    /// Remove a credential, deleting its generations and destroying its secret bytes.
    #[command(alias = "revoke")]
    Remove {
        reference: String,
        #[arg(long, env = "ORBIT_DATABASE_URL_FILE", hide_env_values = true)]
        database_url_file: Option<PathBuf>,
    },
    /// Inspect or explicitly confirm the observed provider account scope for one credential.
    #[command(alias = "scope")]
    ProviderScope {
        #[command(subcommand)]
        action: ProviderScopeAction,
    },
    /// Attach an agy OAuth representation to an existing Antigravity credential.
    AddRepresentation {
        reference: String,
        #[arg(long, required = true)]
        interface: String,
        #[arg(long, default_value = "oauth-personal")]
        auth_type: String,
        /// Qualification-only source import; the path is never echoed or persisted.
        #[arg(long, hide = true)]
        source_file: Option<PathBuf>,
        #[arg(long, env = "ORBIT_DATABASE_URL_FILE", hide_env_values = true)]
        database_url_file: Option<PathBuf>,
    },
    /// Refresh and display safe provider status for one credential or all credentials.
    Status {
        #[arg(
            value_name = "REFERENCE",
            required_unless_present = "all",
            conflicts_with = "all"
        )]
        reference: Option<String>,
        #[arg(long)]
        all: bool,
        /// Provider-grouped detailed quota view for all credentials.
        #[arg(long)]
        quota: bool,
        /// Machine-readable structured JSON output.
        #[arg(long)]
        json: bool,
        /// Full diagnostic and qualification debug output.
        #[arg(long)]
        debug: bool,
        /// Include bounded structural details about files created in the isolated runtime HOME.
        #[arg(long, hide = true)]
        diagnostics: bool,
        #[arg(long, env = "ORBIT_DATABASE_URL_FILE", hide_env_values = true)]
        database_url_file: Option<PathBuf>,
    },
    /// Qualification-only: capture the authorized Antigravity agy `/usage` response.
    #[command(hide = true)]
    CaptureAgyUsage {
        reference: String,
        #[arg(long, env = "ORBIT_DATABASE_URL_FILE", hide_env_values = true)]
        database_url_file: Option<PathBuf>,
    },
    /// Qualification-only: run one catalog-backed Codex status observation.
    #[command(hide = true)]
    ProbeCodexStatus {
        reference: String,
        #[arg(long, env = "ORBIT_DATABASE_URL_FILE", hide_env_values = true)]
        database_url_file: Option<PathBuf>,
    },
    /// List registered credential metadata (operator only; no secrets).
    List,
    /// Inspect one registered credential and its generations (operator only).
    Inspect { reference: String },
}

#[derive(Subcommand)]
enum ProviderScopeAction {
    /// Show the redacted scope fingerprint and enrollment state without a provider call.
    #[command(alias = "show")]
    Inspect {
        reference: String,
        #[arg(long, env = "ORBIT_DATABASE_URL_FILE", hide_env_values = true)]
        database_url_file: Option<PathBuf>,
    },
    /// Confirm that the exact observed fingerprint belongs to this logical credential.
    Confirm {
        reference: String,
        #[arg(long)]
        fingerprint: String,
        #[arg(long, env = "ORBIT_DATABASE_URL_FILE", hide_env_values = true)]
        database_url_file: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum Commands {
    /// Operator credential registry and enrollment.
    Credential(CredentialArgs),
    /// Validate an operator-owned ACP launch policy and print its canonical digest.
    AcpLaunchDigest {
        #[arg(long)]
        config: PathBuf,
    },
    /// Check a pinned ACP installation using initialize only; no login or task execution.
    AcpProbe {
        #[arg(long)]
        config: PathBuf,
        /// Existing disposable directory for a fresh, credential-free probe HOME.
        #[arg(long)]
        workspaces: PathBuf,
    },
    /// Print canonical package digest and domain-separated bytes for external signing.
    PackageDigest {
        manifest: PathBuf,
    },
    /// Publish an already signed package to the configured private registry.
    PublishPackage {
        package: PathBuf,
        #[arg(long)]
        scope: Option<String>,
    },
    Packages {
        #[arg(long)]
        scope: Option<String>,
    },
    Package {
        digest: String,
        #[arg(long)]
        scope: Option<String>,
    },
    Identity,
    Projects,
    Protocol,
    /// Probe process readiness without credentials (suitable for container health checks).
    Health {
        #[arg(long)]
        live: bool,
    },
    /// Stop new assignments to a registered worker; existing attempts keep their leases.
    DrainWorker {
        worker_id: String,
        #[arg(long)]
        resume: bool,
    },
    /// Submit a named definition from a currently trusted, digest-pinned package.
    RunPackage {
        digest: String,
        definition: String,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        request_id: Option<String>,
    },
    Audit {
        #[arg(long, default_value_t = 0)]
        after: u64,
    },
    /// Serve the MCP 2025-11-25 stdio adapter. Credentials come from ORBIT_TOKEN.
    Mcp,
    /// Record an assigned human approval decision (denial fails the run).
    Approve {
        run_id: String,
        step: String,
        #[arg(long)]
        deny: bool,
        #[arg(long, default_value = "")]
        comment: String,
        #[arg(long)]
        request_id: Option<String>,
    },
    #[command(hide = true)]
    ContainerSupervisor {
        #[arg(long)]
        assignment: PathBuf,
    },
    #[command(hide = true)]
    WorkspaceSupervisor {
        #[arg(long)]
        request: PathBuf,
    },
    #[command(hide = true)]
    AcpSupervisor {
        #[arg(long)]
        request: PathBuf,
    },
    /// Export qualification evidence for operator review, excluding runtime fixtures.
    ExportEvidence {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    /// Save a run, its journal and verified accepted artifacts for private review.
    ExportRun {
        run_id: String,
        /// New private directory; existing paths are never overwritten.
        #[arg(long)]
        output: PathBuf,
        /// Maximum total export size, including metadata (default 256 MiB).
        #[arg(long, default_value_t = orbit::run_export::DEFAULT_MAX_BYTES, value_parser = clap::value_parser!(u64).range(1..))]
        max_bytes: u64,
    },
    /// Run saved worker inputs locally, without updating any Orbit run.
    ExecuteLocal {
        #[arg(long)]
        assignment: PathBuf,
        #[arg(long)]
        workspaces: PathBuf,
        #[arg(long)]
        artifacts: PathBuf,
    },
    Server {
        #[arg(
            long,
            env = "DATABASE_URL",
            hide_env_values = true,
            conflicts_with = "database_url_file",
            required_unless_present = "database_url_file"
        )]
        database_url: Option<String>,
        #[arg(long, env = "ORBIT_DATABASE_URL_FILE", hide_env_values = true)]
        database_url_file: Option<PathBuf>,
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        artifacts: PathBuf,
        #[arg(long, default_value = "127.0.0.1:7700")]
        listen: String,
        #[arg(long, default_value_t = 30)]
        lease_seconds: i64,
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
        shutdown_grace_seconds: u64,
    },
    Validate {
        definition: PathBuf,
        #[arg(long)]
        base_revision: Option<String>,
        #[arg(long)]
        task: Option<String>,
    },
    Run(RunArgs),
    Runs {
        #[arg(long, value_enum)]
        output: Option<Output>,
    },
    /// Inspect the shared database scheduler limits.
    Limits,
    Workers,
    Queues,
    /// Replace scheduler limits across all servers using this database.
    SetLimits {
        #[arg(long)]
        max_active_roots: u32,
        #[arg(long)]
        max_running_attempts: u32,
        #[arg(long)]
        max_attempts_per_worker: u32,
    },
    Inspect {
        run_id: String,
        #[arg(long)]
        json: bool,
    },
    Events {
        run_id: String,
        #[arg(long, value_enum)]
        output: Option<Output>,
        /// Exclusive durable journal cursor.
        #[arg(long)]
        after: Option<u64>,
        /// Poll durable events continuously, emitting flushed JSONL records.
        #[arg(long)]
        follow: bool,
    },
    Cancel {
        run_id: String,
    },
    /// Deliver one JSON signal to a named engine.wait step.
    Signal {
        run_id: String,
        step: String,
        #[arg(long)]
        request_id: Option<String>,
        /// JSON file containing a payload of at most 16 KiB. Defaults to null.
        #[arg(long)]
        payload: Option<PathBuf>,
    },
    Worker {
        #[arg(long)]
        capability: String,
        #[arg(long)]
        workspaces: PathBuf,
        #[arg(long)]
        once: bool,
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
        shutdown_grace_seconds: u64,
        #[arg(long)]
        agent_runtime: Option<PathBuf>,
        #[arg(long)]
        execution_config: Option<PathBuf>,
    },
    /// Recover a retained artifact through the operator API.
    Artifact {
        run_id: String,
        artifact_id: String,
        #[arg(long)]
        output: PathBuf,
    },
}

async fn read_private_database_url(file: Option<&Path>) -> Result<Zeroizing<String>> {
    let home = orbit::secret_backend::operator_home()?;
    let path = file
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("ORBIT_DATABASE_URL_FILE").map(PathBuf::from))
        .unwrap_or_else(|| home.join(".orbit/private/database/control-plane-url"));
    anyhow::ensure!(
        path.is_absolute() && path.canonicalize()? == path,
        "database URL file path is not canonical"
    );
    let private_root = home.join(".orbit/private");
    anyhow::ensure!(
        path.starts_with(&private_root),
        "database URL must come from the Orbit private root"
    );
    let orbit_root = private_root.parent().context("Orbit root unavailable")?;
    let mut directory = path.parent().context("database URL parent unavailable")?;
    loop {
        let metadata = fs::symlink_metadata(directory)?;
        anyhow::ensure!(
            metadata.is_dir()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o7777 == 0o700,
            "database URL parent owner or mode invalid"
        );
        if directory == orbit_root {
            break;
        }
        directory = directory
            .parent()
            .context("database URL is outside Orbit private root")?;
        anyhow::ensure!(
            directory.starts_with(&private_root) || directory == orbit_root,
            "database URL parent escaped Orbit private root"
        );
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.nlink() == 1
            && metadata.mode() & 0o7777 == 0o600
            && metadata.len() > 0
            && metadata.len() <= 8192,
        "database URL file owner, mode or size invalid"
    );
    let mut bytes = Zeroizing::new(Vec::with_capacity(metadata.len() as usize));
    file.take(8193).read_to_end(&mut bytes)?;
    let text = std::str::from_utf8(&bytes).context("database URL file is not UTF-8")?;
    let url = text.trim();
    anyhow::ensure!(!url.is_empty(), "database URL file is empty");
    Ok(Zeroizing::new(url.to_owned()))
}

fn validate_durable_catalog_url(database_url: &str) -> Result<()> {
    let options = database_url
        .parse::<sqlx::postgres::PgConnectOptions>()
        .map_err(|_| anyhow::anyhow!("durable catalog URL is invalid"))?;
    anyhow::ensure!(
        options.get_host() == "127.0.0.1"
            && options.get_port() == 55442
            && options.get_database() == Some("orbit_control_plane"),
        "database target is not the configured durable Orbit control-plane catalog"
    );
    Ok(())
}

async fn connect_durable_catalog(database_url: &str) -> Result<sqlx::PgPool> {
    validate_durable_catalog_url(database_url)?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(database_url)
        .await
        .map_err(|_| anyhow::anyhow!("durable credential catalog connection failed"))?;
    let identity = sqlx::query("SELECT current_database() AS database, current_schema() AS schema")
        .fetch_one(&pool)
        .await?;
    let database: String = identity.get("database");
    let schema: String = identity.get("schema");
    anyhow::ensure!(
        database == "orbit_control_plane" && schema == "public",
        "connected database is not the durable Orbit control-plane catalog"
    );
    let schema_ready: bool = sqlx::query_scalar("SELECT to_regclass('orbit_credentials') IS NOT NULL AND to_regclass('orbit_credential_representations') IS NOT NULL AND to_regclass('orbit_availability_snapshots') IS NOT NULL AND to_regclass('orbit_provider_scope_bindings') IS NOT NULL")
        .fetch_one(&pool)
        .await?;
    anyhow::ensure!(
        schema_ready,
        "durable Orbit catalog migrations are not current"
    );
    Ok(pool)
}

async fn connect_durable_catalog_engine(database_url: &str) -> Result<(Engine, tempfile::TempDir)> {
    let preflight = connect_durable_catalog(database_url).await?;
    preflight.close().await;
    let scratch = orbit::codex_status_probe::private_control_tempdir()?;
    let engine = Engine::connect(database_url, scratch.path().join("artifacts"), 30)
        .await
        .map_err(|_| anyhow::anyhow!("durable credential catalog migration failed"))?;
    Ok((engine, scratch))
}

const CREDENTIAL_STATUS_ALL_MAX: usize = 32;
const CREDENTIAL_STATUS_TTL: Duration = Duration::from_secs(60);

fn unix_time_ms() -> Result<i64> {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before Unix epoch")?
            .as_millis(),
    )
    .context("system timestamp is out of range")
}

fn safe_representation_state(
    inspection: &orbit::credential_registry::CredentialInspection,
    interface: &str,
    generation: u64,
) -> &'static str {
    match inspection.representations.iter().find(|representation| {
        representation.interface == interface
            && representation.generation == generation
            && representation.current_generation
    }) {
        None => "missing",
        Some(representation)
            if representation.state == orbit::credential_registry::RepresentationState::Stored
                && representation.validation == "valid" =>
        {
            "valid"
        }
        Some(representation)
            if representation.state == orbit::credential_registry::RepresentationState::Invalid =>
        {
            "invalid"
        }
        Some(representation)
            if representation.state == orbit::credential_registry::RepresentationState::Pending =>
        {
            "pending"
        }
        Some(_) => "unvalidated",
    }
}

fn representation_status_reason(
    inspection: &orbit::credential_registry::CredentialInspection,
    interface: &str,
    generation: u64,
    reference: &str,
) -> String {
    let Some(representation) = inspection.representations.iter().find(|representation| {
        representation.interface == interface
            && representation.generation == generation
            && representation.current_generation
    }) else {
        return format!("{interface} representation is missing");
    };
    match representation.state {
        orbit::credential_registry::RepresentationState::Invalid => {
            format!("{interface} representation is invalid")
        }
        orbit::credential_registry::RepresentationState::Disabled => {
            format!("{interface} representation is disabled")
        }
        orbit::credential_registry::RepresentationState::Revoked => {
            format!("{interface} representation is revoked")
        }
        orbit::credential_registry::RepresentationState::Pending => format!(
            "{interface} representation is pending at stage {}; rerun `orbit credential add-representation {reference} --interface {interface}` to resume",
            representation
                .enrollment_stage
                .as_deref()
                .unwrap_or("unknown")
        ),
        orbit::credential_registry::RepresentationState::Stored
            if representation.validation != "valid" =>
        {
            format!(
                "{interface} representation is unvalidated at stage {}; rerun `orbit credential add-representation {reference} --interface {interface}` to resume",
                representation
                    .enrollment_stage
                    .as_deref()
                    .unwrap_or("unknown")
            )
        }
        orbit::credential_registry::RepresentationState::Stored => {
            format!("{interface} representation is not eligible for this runtime")
        }
    }
}

fn compact_agy_runtime_effects(entries: &[serde_json::Value]) -> serde_json::Value {
    let mut categories = std::collections::BTreeSet::new();
    let observed_runtime_entries = entries
        .iter()
        .filter(|entry| entry["path"] != ".gemini/antigravity-cli/antigravity-oauth-token")
        .count();
    for entry in entries {
        let Some(path) = entry["path"].as_str() else {
            categories.insert("other".to_owned());
            continue;
        };
        let category = if path.starts_with(".gemini/antigravity-cli/builtin/")
            || path == ".gemini/antigravity-cli/builtin"
        {
            "builtins"
        } else if path.starts_with(".gemini/antigravity-cli/cache/")
            || path == ".gemini/antigravity-cli/cache"
        {
            "cache"
        } else if path.starts_with(".gemini/antigravity-cli/log/")
            || path == ".gemini/antigravity-cli/log"
            || path == ".gemini/antigravity-cli/cli.log"
        {
            "logs"
        } else if path.starts_with(".gemini/antigravity-cli/conversations/")
            || path == ".gemini/antigravity-cli/conversations"
            || path == ".gemini/antigravity-cli/conversation_summaries.db"
        {
            "conversation_state"
        } else if path.starts_with(".gemini/antigravity-cli/crashes/")
            || path == ".gemini/antigravity-cli/crashes"
        {
            "crash_state"
        } else if path.starts_with(".gemini/antigravity-cli/brain/")
            || path == ".gemini/antigravity-cli/brain"
            || path == ".gemini/antigravity-cli/installation_id"
            || path == ".gemini/antigravity-cli/jetski_state.pbtxt"
        {
            "runtime_state"
        } else if path.starts_with(".gemini/config/") || path == ".gemini/config" {
            "runtime_configuration"
        } else if path == ".gemini" || path == ".gemini/antigravity-cli" {
            "runtime_directories"
        } else if path == ".gemini/antigravity-cli/antigravity-oauth-token" {
            continue;
        } else {
            "other"
        };
        categories.insert(category.to_owned());
    }
    serde_json::json!({
        "staged_auth_artifact": true,
        "observed_runtime_entries": observed_runtime_entries,
        "runtime_categories": categories,
    })
}

fn snapshot_status(
    snapshot: Option<&orbit::availability::AvailabilitySnapshot>,
    now_ms: i64,
) -> serde_json::Value {
    let Some(snapshot) = snapshot else {
        return serde_json::json!({"state":"unknown","fresh":false,"observed_at_ms":null,"expires_at_ms":null,"quota_buckets":[],"quota_groups":[]});
    };
    let fresh = snapshot.observed_at_ms <= now_ms && snapshot.expires_at_ms > now_ms;
    serde_json::json!({
        "state": if fresh { snapshot.state } else { orbit::availability::AvailabilityState::Unknown },
        "recorded_state": snapshot.state,
        "fresh": fresh,
        "observed_at_ms": snapshot.observed_at_ms,
        "expires_at_ms": snapshot.expires_at_ms,
        "quota_windows": snapshot.quota_windows,
        "quota_buckets": snapshot.quota_buckets,
        "quota_groups": snapshot.quota_groups,
    })
}

fn attach_status_health_dimensions(report: &mut serde_json::Value) {
    let status = &report["status"];
    let availability = &report["availability"];
    let status_state = status["state"].as_str().unwrap_or("unknown");
    let fresh = availability["fresh"].as_bool().unwrap_or(false);
    let quota_buckets = availability["quota_buckets"].as_array().map_or(0, Vec::len);
    let quota_groups = availability["quota_groups"].as_array().map_or(0, Vec::len);
    let quota_windows = availability["quota_windows"].as_array().map_or(0, Vec::len);
    let runtime_state = if status["authenticated"].as_bool() == Some(true) {
        "healthy"
    } else {
        "not_established"
    };
    report["health"] = serde_json::json!({
        "auth": {"representations": report["representations"].clone()},
        "runtime": {"state": runtime_state},
        "provider_scope": {"state": status["provider_scope"].as_str().unwrap_or("none")},
        "status_observation": {
            "state": status_state,
            "observed_now": matches!(status_state, "observed" | "partial"),
            "last_snapshot_fresh": fresh,
            "observed_at_ms": availability["observed_at_ms"],
            "expires_at_ms": availability["expires_at_ms"],
        },
        "quota_evidence": {
            "persisted_bucket_count": quota_buckets,
            "persisted_window_count": quota_windows,
            "persisted_group_count": quota_groups,
        },
        "scheduling_availability": {
            "state": availability["state"],
            "fresh": fresh,
        },
    });
}

async fn credential_status_report(
    pool: &sqlx::PgPool,
    backend: &orbit::secret_backend::LocalPrivateSecretBackend,
    reference: &str,
    diagnostics: bool,
) -> Result<serde_json::Value> {
    let store = CredentialStore::new(pool);
    let Some(credential) = store.get(reference).await? else {
        return Ok(serde_json::json!({
            "reference": reference,
            "status": {"state":"unavailable","reason":"credential not found"}
        }));
    };
    let now_ms = unix_time_ms()?;
    let inspection = store
        .inspect(reference)
        .await?
        .context("credential disappeared during status inspection")?;
    let snapshot = AvailabilityStore::new(pool)
        .current_for_credential(&credential.identity())
        .await?;
    let mut current_representations = serde_json::Map::new();
    for representation in inspection
        .representations
        .iter()
        .filter(|representation| representation.current_generation)
    {
        let secret_artifact = match store.representation(&representation.id).await {
            Ok(Some(stored)) => match stored.secret_locator {
                Some(locator) => match backend.exists(locator).await {
                    Ok(true) => "present",
                    Ok(false) => "missing",
                    Err(_) => "unavailable",
                },
                None => "not_configured",
            },
            _ => "unknown",
        };
        current_representations.insert(
            representation.interface.clone(),
            serde_json::json!({
                "auth_type": representation.auth_type,
                "publication_state": representation.state,
                "validation": representation.validation,
                "enrollment_stage": representation.enrollment_stage,
                "last_validated_at_ms": representation.last_validated_at_ms,
                "catalog_locator_present": representation.has_secret,
                "secret_artifact": secret_artifact,
            }),
        );
    }
    let mut report = serde_json::json!({
        "credential": {
            "reference": credential.reference,
            "id": credential.id,
            "provider": credential.provider,
            "generation": credential.generation,
            "lifecycle": credential.status,
        },
        "representations": current_representations,
        "status": {"state":"unavailable","reason":"status adapter not available"},
        "availability": snapshot_status(snapshot.as_ref(), now_ms),
    });
    let unavailable = |report: &mut serde_json::Value, reason: &str| {
        report["status"] = serde_json::json!({"state":"unavailable","reason":reason});
        attach_status_health_dimensions(report);
    };

    if credential.status != orbit::credential_registry::CredentialStatus::Enrolled {
        let reason = match credential.status {
            orbit::credential_registry::CredentialStatus::Revoked => "credential is revoked",
            orbit::credential_registry::CredentialStatus::Pending => {
                "credential enrollment is pending"
            }
            orbit::credential_registry::CredentialStatus::Invalid => "credential is invalid",
            orbit::credential_registry::CredentialStatus::Disabled => "credential is disabled",
            orbit::credential_registry::CredentialStatus::Enrolled => unreachable!(),
        };
        unavailable(&mut report, reason);
        return Ok(report);
    }
    if credential.secret_backend != backend.backend_id() {
        unavailable(&mut report, "configured SecretBackend is unavailable");
        return Ok(report);
    }

    match credential.provider.as_str() {
        "codex" => {
            match safe_representation_state(
                &inspection,
                orbit::codex_credential_enrollment::CODEX_INTERFACE,
                credential.generation,
            ) {
                "missing" => {
                    unavailable(&mut report, "Codex representation is not enrolled");
                    return Ok(report);
                }
                "valid" => {}
                _ => {
                    unavailable(
                        &mut report,
                        &representation_status_reason(
                            &inspection,
                            orbit::codex_credential_enrollment::CODEX_INTERFACE,
                            credential.generation,
                            reference,
                        ),
                    );
                    return Ok(report);
                }
            }
            let (runtime, resource) =
                match orbit::codex_status_probe::cataloged_codex_runtime(&credential) {
                    Ok(value) => value,
                    Err(_) => {
                        unavailable(&mut report, "Codex runtime configuration is unavailable");
                        return Ok(report);
                    }
                };
            let bindings = orbit::provider_scope::BindingStore::new(pool);
            let existing_binding = match bindings.inspect(&credential.identity()).await {
                Ok(binding) => binding,
                Err(_) => {
                    unavailable(&mut report, "provider-scope state could not be read");
                    return Ok(report);
                }
            };
            let (binding, mode) = match existing_binding.as_ref() {
                Some(binding)
                    if binding.state == orbit::provider_scope::BindingState::Confirmed =>
                {
                    (
                        orbit::codex_status_probe::ProbeBinding::ConfirmedFingerprint(
                            &binding.fingerprint,
                        ),
                        orbit::provider_scope::ObservationMode::Confirmed,
                    )
                }
                _ => (
                    orbit::codex_status_probe::ProbeBinding::Enroll,
                    orbit::provider_scope::ObservationMode::Enrollment,
                ),
            };
            let control = match orbit::codex_status_probe::private_control_tempdir() {
                Ok(control) => control,
                Err(_) => {
                    unavailable(&mut report, "private runtime staging failed");
                    return Ok(report);
                }
            };
            let outcome = match orbit::codex_status_probe::probe_cataloged_once(
                orbit::codex_status_probe::CatalogCredentialSource {
                    pool,
                    backend,
                    reference,
                },
                &runtime,
                &resource,
                binding,
                control.path(),
                CREDENTIAL_STATUS_TTL,
            )
            .await
            {
                Ok(outcome) => outcome,
                Err(failure) => {
                    unavailable(&mut report, failure.kind.safe_message());
                    report["status"]["failure_kind"] = serde_json::to_value(failure.kind)?;
                    return Ok(report);
                }
            };
            if !outcome.receipt.cleanup_confirmed
                || !outcome.receipt.authenticated_account_present
                || !outcome.receipt.correlated_status_response
                || outcome.receipt.model_thread_created
                || outcome.receipt.model_turn_started
            {
                unavailable(&mut report, "Codex status evidence was incomplete");
                return Ok(report);
            }
            let observed_quota_windows =
                outcome
                    .receipt
                    .quota_observation
                    .as_ref()
                    .is_some_and(|observation| {
                        observation.state
                            == orbit::provider_status::CodexRateLimitObservationState::Observed
                    });
            let account_read_schema = outcome.receipt.account_read_schema.clone();
            let rate_limits_schema = outcome.receipt.rate_limits_schema.clone();
            let quota_observation = outcome.receipt.quota_observation.clone();
            let account_identity_value_comparison =
                outcome.receipt.account_identity_value_comparison;
            let recorded = match bindings
                .record_observation(
                    &credential.identity(),
                    outcome.provider_scope_fingerprint.as_deref(),
                    mode,
                    outcome.snapshot,
                )
                .await
            {
                Ok(recorded) => recorded,
                Err(_) => {
                    unavailable(&mut report, "status evidence could not be persisted");
                    return Ok(report);
                }
            };
            let provider_scope = recorded
                .binding
                .as_ref()
                .map(|binding| format!("{:?}", binding.state).to_ascii_lowercase())
                .unwrap_or_else(|| "none".to_owned());
            let scope_fingerprint = recorded.binding.as_ref().map(|binding| {
                if binding.state == orbit::provider_scope::BindingState::Mismatch {
                    binding
                        .mismatch_fingerprint
                        .as_ref()
                        .unwrap_or(&binding.fingerprint)
                } else {
                    &binding.fingerprint
                }
            });
            let quota_observed = serde_json::json!({
                "scope":"codex-provider-status",
                "buckets":recorded.snapshot.quota_buckets,
                "legacy_windows":recorded.snapshot.quota_windows,
            });
            let provider_status_observation =
                recorded.snapshot.provider_status_observation.as_ref();
            let quota_promoted = provider_status_observation.is_some_and(|observation| {
                observation.quota_promotion == orbit::availability::QuotaEvidencePromotion::Promoted
            });
            let quota_promotion_reason = provider_status_observation
                .map(|observation| serde_json::to_value(observation.quota_promotion))
                .transpose()?
                .unwrap_or_else(|| serde_json::json!("no_usable_windows_observed"));
            let quota_evidence_persisted = !recorded.snapshot.quota_buckets.is_empty()
                || !recorded.snapshot.quota_windows.is_empty();
            report["status"] = serde_json::json!({
                "state":"observed",
                "request_count":1,
                "authenticated":true,
                "account_read":true,
                "rate_limits_read":true,
                "fresh_backend_representation_staged":true,
                "model_turn":false,
                "account_read_schema":account_read_schema,
                "rate_limits_read_schema":rate_limits_schema,
                "provider_quota_observation":quota_observation,
                "usable_quota_windows_observed":observed_quota_windows,
                "account_identity_value_comparison":account_identity_value_comparison,
                "quota_promoted":quota_promoted,
                "quota_promotion_reason":quota_promotion_reason,
                "snapshot_id":recorded.snapshot_id,
                "provider_scope":provider_scope,
                "provider_scope_fingerprint":scope_fingerprint,
                "quota_observed":quota_observed,
                "quota_persisted":quota_evidence_persisted,
                "scheduling_availability":recorded.snapshot.state,
            });
            report["availability"] = snapshot_status(Some(&recorded.snapshot), unix_time_ms()?);
        }
        "antigravity" => {
            match safe_representation_state(
                &inspection,
                orbit::agy_cli_representation::AGY_CLI_INTERFACE,
                credential.generation,
            ) {
                "missing" => {
                    report["representations"]["agy-cli"] =
                        serde_json::json!({"auth_type":"oauth-personal","state":"missing"});
                    unavailable(&mut report, "agy-cli representation not enrolled");
                    return Ok(report);
                }
                "valid" => {}
                _ => {
                    unavailable(
                        &mut report,
                        &representation_status_reason(
                            &inspection,
                            orbit::agy_cli_representation::AGY_CLI_INTERFACE,
                            credential.generation,
                            reference,
                        ),
                    );
                    return Ok(report);
                }
            }
            let receipt = match orbit::agy_cli_representation::capture_usage_for_credential(
                pool, backend, reference,
            )
            .await
            {
                Ok(receipt) => receipt,
                Err(_) => {
                    unavailable(
                        &mut report,
                        "agy status staging, authentication or provider request failed",
                    );
                    return Ok(report);
                }
            };
            if !receipt.status.normalization_ready {
                report["status"] = serde_json::json!({
                    "state":"partial",
                    "request_count":1,
                    "authenticated":true,
                    "model_turn":false,
                    "reason":"provider status could not be normalized safely",
                    "schema_summary":receipt.status.summary,
                });
                attach_status_health_dimensions(&mut report);
                return Ok(report);
            }
            let observed_at_ms = unix_time_ms()?;
            let expires_at_ms = observed_at_ms + CREDENTIAL_STATUS_TTL.as_millis() as i64;
            let snapshot = match receipt.status.availability_snapshot(
                credential.identity(),
                observed_at_ms,
                expires_at_ms,
            ) {
                Ok(snapshot) => snapshot,
                Err(_) => {
                    unavailable(&mut report, "normalized agy status evidence was invalid");
                    return Ok(report);
                }
            };
            let snapshot_id = match AvailabilityStore::new(pool).record(&snapshot).await {
                Ok(id) => id,
                Err(_) => {
                    unavailable(&mut report, "status evidence could not be persisted");
                    return Ok(report);
                }
            };
            report["status"] = serde_json::json!({
                "state":"observed",
                "request_count":1,
                "authenticated":true,
                "model_turn":false,
                "snapshot_id":snapshot_id,
                "normalization_ready":receipt.status.normalization_ready,
                "group_metadata_ready":receipt.status.group_metadata_ready,
                "membership_ready":receipt.status.membership_ready,
                "token_file_metadata_changed":receipt.token_metadata_changed,
                "schema_summary":receipt.status.summary,
                "runtime_effects":compact_agy_runtime_effects(&receipt.created_or_changed_home_entries),
            });
            if diagnostics {
                report["status"]["home_entries"] =
                    serde_json::json!(receipt.created_or_changed_home_entries);
            }
            report["availability"] = snapshot_status(Some(&snapshot), unix_time_ms()?);
        }
        _ => unavailable(
            &mut report,
            "no status adapter is available for this provider",
        ),
    }
    attach_status_health_dimensions(&mut report);
    Ok(report)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut output_format = cli.output_format;
    if let Commands::AcpLaunchDigest { config } = &cli.command {
        use tokio::io::AsyncReadExt;
        let mut bytes = Vec::new();
        tokio::fs::File::open(config)
            .await?
            .take(65537)
            .read_to_end(&mut bytes)
            .await?;
        anyhow::ensure!(
            bytes.len() <= 65536,
            "ACP launch configuration exceeds 64 KiB"
        );
        let launch: orbit::acp_runtime::Launch = serde_json::from_slice(&bytes)?;
        launch.validate()?;
        println!("{}", serde_json::json!({"launch_digest":launch.digest()?}));
        return Ok(());
    }
    if let Commands::AcpSupervisor { request } = &cli.command {
        let code = match orbit::acp_process::supervise(request).await {
            Ok(code) => code,
            Err(err) => {
                eprintln!("ACP supervisor failed: {err:#}");
                1
            }
        };
        std::process::exit(code);
    }
    if let Commands::AcpProbe { config, workspaces } = &cli.command {
        use tokio::io::AsyncReadExt;
        let mut bytes = Vec::new();
        tokio::fs::File::open(config)
            .await?
            .take(65537)
            .read_to_end(&mut bytes)
            .await?;
        anyhow::ensure!(
            bytes.len() <= 65536,
            "ACP probe configuration exceeds 64 KiB"
        );
        let config: orbit::acp::ProbeConfig = serde_json::from_slice(&bytes)?;
        let value = orbit::acp::probe(&config, workspaces).await?;
        match output_format {
            Output::Json | Output::Text => println!("{}", serde_json::to_string_pretty(&value)?),
            Output::Jsonl => println!("{}", serde_json::to_string(&value)?),
        }
        return Ok(());
    }
    if let Commands::Credential(CredentialArgs {
        action:
            CredentialAction::Rename {
                old_reference,
                new_reference,
                database_url_file,
            },
    }) = &cli.command
    {
        let database_url = read_private_database_url(database_url_file.as_deref()).await?;
        let (engine, scratch) = connect_durable_catalog_engine(database_url.as_str()).await?;
        let renamed = CredentialStore::new(&engine.pool)
            .rename(old_reference, new_reference)
            .await?;
        let summary = serde_json::json!({
            "credential_id": renamed.id,
            "old_reference": old_reference,
            "new_reference": renamed.reference,
            "provider": renamed.provider,
            "generation": renamed.generation,
            "lifecycle": renamed.status,
        });
        match output_format {
            Output::Json | Output::Text => println!("{}", serde_json::to_string_pretty(&summary)?),
            Output::Jsonl => println!("{}", serde_json::to_string(&summary)?),
        }
        engine.pool.close().await;
        drop(scratch);
        return Ok(());
    }
    if let Commands::Credential(CredentialArgs {
        action:
            CredentialAction::Remove {
                reference,
                database_url_file,
            },
    }) = &cli.command
    {
        let database_url = read_private_database_url(database_url_file.as_deref()).await?;
        let (engine, scratch) = connect_durable_catalog_engine(database_url.as_str()).await?;
        let backend = orbit::secret_backend::LocalPrivateSecretBackend::default_for_operator()?;
        let removed = CredentialStore::new(&engine.pool).hard_delete(reference, &backend).await?;
        let summary = serde_json::json!({
            "credential_id": removed.id,
            "reference": removed.reference,
            "provider": removed.provider,
            "generation": removed.generation,
            "lifecycle": "deleted",
            "secret_bytes_destroyed": true,
        });
        match output_format {
            Output::Json | Output::Text => println!("{}", serde_json::to_string_pretty(&summary)?),
            Output::Jsonl => println!("{}", serde_json::to_string(&summary)?),
        }
        engine.pool.close().await;
        drop(scratch);
        return Ok(());
    }
    if let Commands::Credential(CredentialArgs {
        action:
            CredentialAction::Add {
                provider,
                name,
                auth_method,
                database_url_file,
            },
    }) = &cli.command
    {
        anyhow::ensure!(
            matches!(provider.as_str(), "antigravity" | "codex"),
            "provider enrollment is not implemented"
        );
        let reference = if let Some(name) = name {
            name.clone()
        } else {
            use std::io::{self, Write};
            eprint!("Credential name: ");
            io::stderr().flush()?;
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            input.trim().to_owned()
        };
        anyhow::ensure!(
            orbit::credential_registry::valid_reference(&reference),
            "invalid credential name"
        );
        let chosen = if let Some(method) = auth_method {
            method.clone()
        } else if provider == "codex" {
            orbit::codex_credential_enrollment::CODEX_AUTH_TYPE.to_owned()
        } else {
            use std::io::{self, Write};
            eprintln!(
                "Authentication:\n  1. Google Personal\n  2. Gemini Enterprise (not yet enabled)\n  3. Gemini API key (not yet enabled)\n  4. Agent Platform (not yet enabled)"
            );
            eprint!("> ");
            io::stderr().flush()?;
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            match input.trim() {
                "1" => "oauth-personal".to_string(),
                _ => anyhow::bail!("authentication method not enabled"),
            }
        };
        let expected_auth = if provider == "codex" {
            orbit::codex_credential_enrollment::CODEX_AUTH_TYPE
        } else {
            "oauth-personal"
        };
        anyhow::ensure!(chosen == expected_auth, "authentication method not enabled");
        let url = read_private_database_url(database_url_file.as_deref()).await?;
        let pool = connect_durable_catalog(url.as_str()).await?;
        let summary = if provider == "codex" {
            eprintln!("Starting Codex device-code authentication...");
            let enrolled =
                orbit::codex_credential_enrollment::enroll_codex_chatgpt(&pool, &reference).await?;
            serde_json::json!({
                "reference": enrolled.reference,
                "credential_id": enrolled.credential_id,
                "provider": "codex",
                "generation": enrolled.generation,
                "auth_type": expected_auth,
                "lifecycle": enrolled.credential_status,
                "representation": {
                    "interface": orbit::codex_credential_enrollment::CODEX_INTERFACE,
                    "state": enrolled.representation_state,
                    "validation": enrolled.validation,
                },
                "runtime": {
                    "version": orbit::codex_credential_enrollment::CODEX_VERSION,
                    "image_digest": orbit::codex_credential_enrollment::CODEX_IMAGE_DIGEST,
                    "binary_sha256": orbit::codex_credential_enrollment::CODEX_BINARY_SHA256,
                }
            })
        } else {
            eprintln!("Starting Antigravity authentication...");
            let enrolled =
                orbit::credential_enrollment::enroll_antigravity_personal(&pool, &reference)
                    .await?;
            serde_json::json!({"reference":enrolled.reference,"provider":enrolled.provider,
                "generation":enrolled.generation,"auth_type":enrolled.auth_type,"status":enrolled.status})
        };
        match output_format {
            Output::Json | Output::Text => println!("{}", serde_json::to_string_pretty(&summary)?),
            Output::Jsonl => println!("{}", serde_json::to_string(&summary)?),
        }
        return Ok(());
    }
    if let Commands::Credential(CredentialArgs {
        action:
            CredentialAction::AddRepresentation {
                reference,
                interface,
                auth_type,
                source_file,
                database_url_file,
            },
    }) = &cli.command
    {
        anyhow::ensure!(
            orbit::credential_registry::valid_reference(reference)
                && interface == orbit::agy_cli_representation::AGY_CLI_INTERFACE
                && auth_type == orbit::agy_cli_representation::AGY_CLI_AUTH_TYPE,
            "only the agy-cli oauth-personal representation is enabled"
        );
        let database_url = read_private_database_url(database_url_file.as_deref()).await?;
        validate_durable_catalog_url(database_url.as_str())?;
        let preflight_pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(database_url.as_str())
            .await
            .map_err(|_| anyhow::anyhow!("durable credential catalog connection failed"))?;
        let identity = sqlx::query("SELECT current_database() AS database, current_schema() AS schema, inet_server_addr()::text AS host, inet_server_port() AS port")
            .fetch_one(&preflight_pool)
            .await?;
        let database: String = identity.get("database");
        let schema: String = identity.get("schema");
        let host: Option<String> = identity.get("host");
        let port: Option<i32> = identity.get("port");
        let lowered = database.to_ascii_lowercase();
        anyhow::ensure!(
            schema == "public"
                && !["test", "qual", "disposable", "temp"]
                    .iter()
                    .any(|marker| lowered.contains(marker))
                && port != Some(55439),
            "connected database is not the durable Orbit control-plane catalog"
        );
        let _non_secret_database_identity = (database, schema, host, port);
        preflight_pool.close().await;
        let scratch = orbit::codex_status_probe::private_control_tempdir()?;
        let engine = Engine::connect(database_url.as_str(), scratch.path().join("artifacts"), 30)
            .await
            .map_err(|_| {
                anyhow::anyhow!("durable credential catalog connection or migration failed")
            })?;
        let backend = orbit::secret_backend::LocalPrivateSecretBackend::default_for_operator()?;
        let result = if let Some(source_file) = source_file {
            orbit::agy_cli_representation::import_and_validate(
                &engine.pool,
                &backend,
                reference,
                source_file,
            )
            .await?
        } else {
            orbit::agy_cli_representation::enroll_existing_agy_cli(
                &engine.pool,
                &backend,
                reference,
            )
            .await?
        };
        let summary = serde_json::json!({
            "credential": {
                "id": result.credential_id,
                "reference": result.reference,
                "provider": "antigravity",
                "generation": result.generation,
                "lifecycle": result.credential_status,
            },
            "representation": {
                "interface": "agy-cli",
                "auth_type": result.auth_type,
                "state": result.representation_state,
                "validation": result.validation,
            },
            "runtime": {
                "version": result.runtime_version,
                "sha256": result.runtime_sha256,
                "provenance": result.runtime_provenance,
            },
            "identity_binding": result.identity_binding,
            "availability": "unknown",
        });
        match output_format {
            Output::Json | Output::Text => println!("{}", serde_json::to_string_pretty(&summary)?),
            Output::Jsonl => println!("{}", serde_json::to_string(&summary)?),
        }
        engine.pool.close().await;
        return Ok(());
    }
    if let Commands::Credential(CredentialArgs {
        action: CredentialAction::ProviderScope { action },
    }) = &cli.command
    {
        let (reference, database_url_file) = match action {
            ProviderScopeAction::Inspect {
                reference,
                database_url_file,
            }
            | ProviderScopeAction::Confirm {
                reference,
                database_url_file,
                ..
            } => (reference, database_url_file),
        };
        anyhow::ensure!(
            orbit::credential_registry::valid_reference(reference),
            "invalid credential reference"
        );
        let database_url = read_private_database_url(database_url_file.as_deref()).await?;
        let pool = connect_durable_catalog(database_url.as_str()).await?;
        let store = CredentialStore::new(&pool);
        let credential = store
            .get(reference)
            .await?
            .context("credential not found")?;
        anyhow::ensure!(
            credential.provider == "codex",
            "provider-scope CLI is currently enabled for Codex credentials only"
        );
        anyhow::ensure!(
            credential.status == orbit::credential_registry::CredentialStatus::Enrolled,
            "credential is not enrolled"
        );
        let bindings = orbit::provider_scope::BindingStore::new(&pool);
        let identity = credential.identity();
        let summary = match action {
            ProviderScopeAction::Inspect { .. } => {
                let binding = bindings.inspect(&identity).await?;
                let history = bindings.history(&identity).await?;
                let observed = history.iter().find(|event| event.event_kind == "observed");
                let latest_snapshot = orbit::availability::AvailabilityStore::new(&pool)
                    .current_for_credential(&identity)
                    .await?;
                let observation = latest_snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.provider_status_observation.as_ref());
                serde_json::json!({
                    "credential": {
                        "reference": credential.reference,
                        "id": credential.id,
                        "provider": credential.provider,
                        "generation": credential.generation,
                        "lifecycle": credential.status,
                    },
                    "provider_scope": binding.as_ref().map(|binding| serde_json::json!({
                        "state": binding.state,
                        "fingerprint": if binding.state == orbit::provider_scope::BindingState::Mismatch {
                            binding.mismatch_fingerprint.as_ref().unwrap_or(&binding.fingerprint)
                        } else { &binding.fingerprint },
                        "observed_at_ms": observed.map(|event| event.recorded_at_ms),
                        "identity_value_comparison": observation.map(|value| value.identity_value_comparison),
                        "quota_promotion": observation.map(|value| value.quota_promotion),
                    })).unwrap_or(serde_json::Value::Null),
                })
            }
            ProviderScopeAction::Confirm { fingerprint, .. } => {
                let confirmed = bindings
                    .confirm(&identity, fingerprint, "operator-cli")
                    .await?;
                serde_json::json!({
                    "credential": {
                        "reference": credential.reference,
                        "id": credential.id,
                        "provider": credential.provider,
                        "generation": credential.generation,
                        "lifecycle": credential.status,
                    },
                    "provider_scope": {
                        "state": confirmed.state,
                        "fingerprint": confirmed.fingerprint,
                        "confirmed": true,
                    },
                    "status_probe_required": true,
                })
            }
        };
        match output_format {
            Output::Json | Output::Text => println!("{}", serde_json::to_string_pretty(&summary)?),
            Output::Jsonl => println!("{}", serde_json::to_string(&summary)?),
        }
        pool.close().await;
        return Ok(());
    }
    if let Commands::Credential(CredentialArgs {
        action:
            CredentialAction::Status {
                reference,
                all,
                quota,
                json,
                debug,
                diagnostics,
                database_url_file,
            },
    }) = &cli.command
    {
        let database_url = read_private_database_url(database_url_file.as_deref()).await?;
        let pool = connect_durable_catalog(database_url.as_str()).await?;
        let backend = orbit::secret_backend::LocalPrivateSecretBackend::default_for_operator()?;
        let diagnostics_flag = *diagnostics || *debug;
        let now_ms = unix_time_ms()?;
        if *all {
            let credentials = CredentialStore::new(&pool).list().await?;
            anyhow::ensure!(
                credentials.len() <= CREDENTIAL_STATUS_ALL_MAX,
                "credential status --all is bounded to 32 credentials; specify one reference instead"
            );
            let mut reports = Vec::with_capacity(credentials.len());
            for credential in credentials {
                let report = credential_status_report(
                    &pool,
                    &backend,
                    &credential.reference,
                    diagnostics_flag,
                )
                .await
                .unwrap_or_else(|_| serde_json::json!({
                    "credential":{"reference":credential.reference,"provider":credential.provider,"generation":credential.generation,"lifecycle":credential.status},
                    "status":{"state":"unavailable","reason":"credential catalog or status observation failed"}
                }));
                reports.push(report);
            }
            if *debug {
                let summary = serde_json::json!({
                    "count": reports.len(),
                    "maximum_provider_observations": CREDENTIAL_STATUS_ALL_MAX,
                    "credentials": reports,
                });
                if output_format == Output::Jsonl {
                    println!("{}", serde_json::to_string(&summary)?);
                } else {
                    println!("{}", serde_json::to_string_pretty(&summary)?);
                }
            } else if *json || output_format == Output::Jsonl {
                let clean_reports: Vec<serde_json::Value> = reports
                    .into_iter()
                    .map(orbit::credential_status_view::clean_structured_json)
                    .collect();
                let summary = serde_json::json!({
                    "count": clean_reports.len(),
                    "maximum_provider_observations": CREDENTIAL_STATUS_ALL_MAX,
                    "credentials": clean_reports,
                });
                if output_format == Output::Jsonl {
                    println!("{}", serde_json::to_string(&summary)?);
                } else {
                    println!("{}", serde_json::to_string_pretty(&summary)?);
                }
            } else if *quota {
                print!(
                    "{}",
                    orbit::credential_status_view::format_all_quota(
                        &reports,
                        now_ms,
                        orbit::credential_status_view::terminal_width(),
                    )
                );
            } else {
                print!(
                    "{}",
                    orbit::credential_status_view::format_all_overview(&reports, now_ms)
                );
            }
        } else {
            let reference = reference
                .as_deref()
                .context("credential reference is required unless --all is used")?;
            anyhow::ensure!(
                orbit::credential_registry::valid_reference(reference),
                "invalid credential reference"
            );
            let report =
                credential_status_report(&pool, &backend, reference, diagnostics_flag).await?;
            if *debug {
                if output_format == Output::Jsonl {
                    println!("{}", serde_json::to_string(&report)?);
                } else {
                    println!("{}", serde_json::to_string_pretty(&report)?);
                }
            } else if *json || output_format == Output::Jsonl {
                let clean_report = orbit::credential_status_view::clean_structured_json(report);
                if output_format == Output::Jsonl {
                    println!("{}", serde_json::to_string(&clean_report)?);
                } else {
                    println!("{}", serde_json::to_string_pretty(&clean_report)?);
                }
            } else {
                print!(
                    "{}",
                    orbit::credential_status_view::format_single_credential(&report, now_ms)
                );
            }
        }
        pool.close().await;
        return Ok(());
    }
    if let Commands::Credential(CredentialArgs {
        action:
            CredentialAction::CaptureAgyUsage {
                reference,
                database_url_file,
            },
    }) = &cli.command
    {
        let database_url = read_private_database_url(database_url_file.as_deref()).await?;
        let pool = connect_durable_catalog(database_url.as_str()).await?;
        let backend = orbit::secret_backend::LocalPrivateSecretBackend::default_for_operator()?;
        let receipt = orbit::agy_cli_representation::capture_registered_usage_once(
            &pool, &backend, reference,
        )
        .await?;
        let persisted_snapshot = if receipt.status.normalization_ready
            && receipt.status.group_metadata_ready
        {
            let credential = CredentialStore::new(&pool)
                .get(reference)
                .await?
                .context("registered credential disappeared after status capture")?;
            let observed_at_ms = i64::try_from(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .context("system clock is before Unix epoch")?
                    .as_millis(),
            )
            .context("status observation timestamp is out of range")?;
            // Match the existing local provider-status qualification freshness
            // window. Provider reset timestamps remain independent evidence.
            let expires_at_ms = observed_at_ms + 60_000;
            let snapshot = receipt.status.availability_snapshot(
                credential.identity(),
                observed_at_ms,
                expires_at_ms,
            )?;
            let snapshot_id = AvailabilityStore::new(&pool)
                .record(&snapshot)
                .await
                .map_err(|_| anyhow::anyhow!("normalized provider status persistence failed"))?;
            Some((snapshot_id, snapshot))
        } else {
            None
        };
        let summary = serde_json::json!({
            "status_request_count": 1,
            "command": "agy --print \"/usage\" --output-format json",
            "authenticated": true,
            "model_turn": false,
            "raw_response_retained": false,
            "response_bytes": receipt.response_bytes,
            "schema_summary": receipt.status.summary,
            "quota_buckets": receipt.status.quota_buckets,
            "quota_groups": receipt.status.quota_groups,
            "normalization_ready": receipt.status.normalization_ready,
            "group_metadata_ready": receipt.status.group_metadata_ready,
            "membership_ready": receipt.status.membership_ready,
            "normalized_snapshot_stored": persisted_snapshot.is_some(),
            "snapshot_id": persisted_snapshot.as_ref().map(|(id, _)| id),
            "availability": persisted_snapshot.as_ref().map(|(_, snapshot)| serde_json::json!({
                "scope": "credential",
                "state": snapshot.state,
                "observed_at_ms": snapshot.observed_at_ms,
                "expires_at_ms": snapshot.expires_at_ms,
            })),
            "extraction_diagnostics": receipt.status.extraction_diagnostics,
            "token_file_metadata_changed": receipt.token_metadata_changed,
            "home_entries": receipt.created_or_changed_home_entries,
        });
        match output_format {
            Output::Json | Output::Text => println!("{}", serde_json::to_string_pretty(&summary)?),
            Output::Jsonl => println!("{}", serde_json::to_string(&summary)?),
        }
        pool.close().await;
        return Ok(());
    }
    if let Commands::Credential(CredentialArgs {
        action:
            CredentialAction::ProbeCodexStatus {
                reference,
                database_url_file,
            },
    }) = &cli.command
    {
        anyhow::ensure!(
            orbit::credential_registry::valid_reference(reference),
            "invalid credential reference"
        );
        let database_url = read_private_database_url(database_url_file.as_deref()).await?;
        let pool = connect_durable_catalog(database_url.as_str()).await?;
        let credential = CredentialStore::new(&pool)
            .get(reference)
            .await?
            .context("credential is missing from the durable catalog")?;
        let (runtime, resource) = orbit::codex_status_probe::cataloged_codex_runtime(&credential)?;
        let backend = orbit::secret_backend::LocalPrivateSecretBackend::default_for_operator()?;
        let control = orbit::codex_status_probe::private_control_tempdir()?;
        let outcome = orbit::codex_status_probe::probe_cataloged_once(
            orbit::codex_status_probe::CatalogCredentialSource {
                pool: &pool,
                backend: &backend,
                reference,
            },
            &runtime,
            &resource,
            orbit::codex_status_probe::ProbeBinding::Enroll,
            control.path(),
            Duration::from_secs(60),
        )
        .await
        .map_err(|failure| anyhow::anyhow!("Codex catalog status probe failed: {failure}"))?;
        anyhow::ensure!(
            outcome.receipt.cleanup_confirmed
                && outcome.receipt.authenticated_account_present
                && outcome.receipt.correlated_status_response
                && !outcome.receipt.model_thread_created
                && !outcome.receipt.model_turn_started,
            "Codex status qualification evidence incomplete"
        );
        let normalized_bucket_count = outcome.snapshot.quota_buckets.len();
        let recorded = orbit::provider_scope::BindingStore::new(&pool)
            .record_observation(
                &credential.identity(),
                outcome.provider_scope_fingerprint.as_deref(),
                orbit::provider_scope::ObservationMode::Enrollment,
                outcome.snapshot,
            )
            .await
            .map_err(|_| anyhow::anyhow!("Codex status evidence persistence failed"))?;
        anyhow::ensure!(
            recorded.snapshot.state == orbit::availability::AvailabilityState::Unknown,
            "unconfirmed Codex status observation promoted availability"
        );
        let provider_scope = recorded
            .binding
            .as_ref()
            .map(|binding| format!("{:?}", binding.state).to_ascii_lowercase())
            .unwrap_or_else(|| "none".into());
        let summary = serde_json::json!({
            "credential": {
                "reference": credential.reference,
                "id": credential.id,
                "generation": credential.generation,
                "lifecycle": "enrolled",
            },
            "representation": {
                "interface": "codex",
                "auth_type": orbit::codex_credential_enrollment::CODEX_AUTH_TYPE,
                "validation": "valid",
                "artifact": orbit::codex_credential_enrollment::CODEX_AUTH_RELATIVE,
            },
            "runtime": {
                "app_server": orbit::codex_credential_enrollment::CODEX_VERSION,
                "image_digest": orbit::codex_credential_enrollment::CODEX_IMAGE_DIGEST,
                "binary_sha256": orbit::codex_credential_enrollment::CODEX_BINARY_SHA256,
            },
            "status": {
                "request_count": 1,
                "authenticated": outcome.receipt.authenticated_account_present,
                "account_read": outcome.receipt.authenticated_account_present,
                "rate_limits_read": outcome.receipt.correlated_status_response,
                "fresh_backend_auth_staged": outcome.receipt.isolated_auth_staged,
                "model_turn": false,
                "normalized_quota_bucket_count": normalized_bucket_count,
                "availability_snapshot_stored": true,
                "snapshot_id": recorded.snapshot_id,
                "availability": recorded.snapshot.state,
                "availability_scope": "credential",
                "observed_at_ms": recorded.snapshot.observed_at_ms,
                "expires_at_ms": recorded.snapshot.expires_at_ms,
                "persisted_quota_bucket_count": recorded.snapshot.quota_buckets.len(),
                "provider_scope_identity_observed": outcome.provider_scope_fingerprint.is_some(),
                "provider_scope": provider_scope,
            }
        });
        match output_format {
            Output::Json | Output::Text => println!("{}", serde_json::to_string_pretty(&summary)?),
            Output::Jsonl => println!("{}", serde_json::to_string(&summary)?),
        }
        pool.close().await;
        return Ok(());
    }
    let credential_command = matches!(&cli.command, Commands::Credential(_));
    let token = if let Some(path) = cli.token_file {
        anyhow::ensure!(
            cli.token.is_empty(),
            "choose ORBIT_TOKEN or ORBIT_TOKEN_FILE"
        );
        orbit::governance::SecretRef::File { path }.resolve()?
    } else {
        cli.token
    };
    let client = Client::new(cli.url, token)?;
    let value = match cli.command {
        Commands::Credential(args) => match args.action {
            CredentialAction::Add { .. } => {
                unreachable!("local enrollment handled before API credential resolution")
            }
            CredentialAction::Rename { .. } | CredentialAction::Remove { .. } => {
                unreachable!("local credential lifecycle handled before API credential resolution")
            }
            CredentialAction::AddRepresentation { .. } => {
                unreachable!("local representation import handled before API credential resolution")
            }
            CredentialAction::Status { .. } => {
                unreachable!("local credential status handled before API credential resolution")
            }
            CredentialAction::CaptureAgyUsage { .. } => {
                unreachable!("local agy status capture handled before API credential resolution")
            }
            CredentialAction::ProbeCodexStatus { .. } => {
                unreachable!("local Codex status probe handled before API credential resolution")
            }
            CredentialAction::ProviderScope { .. } => {
                unreachable!(
                    "local provider-scope command handled before API credential resolution"
                )
            }
            CredentialAction::List => client.get("/credentials").await?,
            CredentialAction::Inspect { reference } => {
                anyhow::ensure!(
                    orbit::credential_registry::valid_reference(&reference),
                    "invalid credential reference"
                );
                client.get(&format!("/credentials/{reference}")).await?
            }
        },
        Commands::AcpLaunchDigest { .. } => {
            unreachable!("local launch digest handled before credentials")
        }
        Commands::AcpProbe { .. } => {
            unreachable!("local probe handled before API credential resolution")
        }
        Commands::AcpSupervisor { .. } => {
            unreachable!("supervisor handled before credential resolution")
        }
        Commands::PackageDigest { manifest } => {
            let manifest: orbit::registry::Manifest =
                serde_json::from_slice(&tokio::fs::read(manifest).await?)?;
            manifest.validate()?;
            serde_json::json!({"digest":manifest.digest()?,"signing_message_hex":hex::encode(manifest.signing_message()?)})
        }
        Commands::PublishPackage { package, scope } => {
            let package: orbit::registry::Package =
                serde_json::from_slice(&tokio::fs::read(package).await?)?;
            let scope = scope
                .as_deref()
                .map(orbit::governance::Scope::parse)
                .transpose()?;
            client
                .post(
                    "/packages",
                    &serde_json::json!({"package":package,"scope":scope}),
                )
                .await?
        }
        Commands::Packages { scope } => {
            let suffix = scope_query(scope.as_deref())?;
            client.get(&format!("/packages{suffix}")).await?
        }
        Commands::Package { digest, scope } => {
            anyhow::ensure!(
                digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid package digest"
            );
            let suffix = scope_query(scope.as_deref())?;
            client.get(&format!("/packages/{digest}{suffix}")).await?
        }
        Commands::Identity => client.get("/identity").await?,
        Commands::Projects => client.get("/projects").await?,
        Commands::Protocol => client.get("/protocol").await?,
        Commands::Health { live } => {
            tokio::time::timeout(
                Duration::from_secs(3),
                client.get(if live { "/healthz" } else { "/readyz" }),
            )
            .await??
        }
        Commands::DrainWorker { worker_id, resume } => {
            anyhow::ensure!(orbit::agent::valid_name(&worker_id), "invalid worker ID");
            client
                .post(
                    &format!("/workers/{worker_id}/drain"),
                    &serde_json::json!({"draining":!resume}),
                )
                .await?
        }
        Commands::RunPackage {
            digest,
            definition,
            scope,
            request_id,
        } => {
            anyhow::ensure!(
                digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid package digest"
            );
            let suffix = scope_query(scope.as_deref())?;
            let package = client.get(&format!("/packages/{digest}{suffix}")).await?;
            anyhow::ensure!(
                package["verified"] == true && package["package"]["digest"] == digest,
                "package verification unavailable"
            );
            let definition: Definition = serde_json::from_value(
                package["package"]["manifest"]["definitions"]
                    .get(&definition)
                    .ok_or_else(|| anyhow::anyhow!("packaged definition not found"))?
                    .clone(),
            )?;
            let request_id = request_id.unwrap_or_else(id);
            eprintln!("submission request_id={request_id} package_digest={digest}");
            client
                .post(
                    "/runs",
                    &Submit {
                        request_id,
                        definition,
                        scope: scope
                            .as_deref()
                            .map(orbit::governance::Scope::parse)
                            .transpose()?,
                        parent_run_id: None,
                    },
                )
                .await?
        }
        Commands::Audit { after } => client.get(&format!("/audit?after={after}")).await?,
        Commands::Mcp => return orbit::mcp::serve(client).await,
        Commands::Approve {
            run_id,
            step,
            deny,
            comment,
            request_id,
        } => {
            let request_id = request_id.unwrap_or_else(id);
            eprintln!("approval request_id={request_id}");
            client
                .post(
                    &format!("/runs/{run_id}/approvals"),
                    &orbit::agent::Approval {
                        request_id,
                        step,
                        approved: !deny,
                        comment,
                    },
                )
                .await?
        }
        Commands::ContainerSupervisor { assignment } => {
            let result = orbit::container::supervise(&assignment).await;
            let code = match result {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("container supervisor: {error}");
                    125
                }
            };
            std::process::exit(code);
        }
        Commands::WorkspaceSupervisor { request } => {
            let code = match orbit::workspace::supervise(&request).await {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("workspace supervisor: {error}");
                    125
                }
            };
            std::process::exit(code);
        }
        Commands::ExportEvidence { source, output } => orbit::evidence::export(&source, &output)?,
        Commands::ExecuteLocal {
            assignment,
            workspaces,
            artifacts,
        } => {
            worker::execute_local(
                serde_json::from_slice(&tokio::fs::read(assignment).await?)?,
                workspaces,
                artifacts,
            )
            .await?
        }
        Commands::Server {
            database_url,
            database_url_file,
            config,
            artifacts,
            listen,
            lease_seconds,
            shutdown_grace_seconds,
        } => {
            let config: Config = serde_json::from_slice(&tokio::fs::read(config).await?)?;
            let database_url = if let Some(path) = database_url_file {
                orbit::governance::SecretRef::File { path }.resolve()?
            } else {
                database_url.context("database URL required")?
            };
            let engine = Engine::connect(&database_url, artifacts, lease_seconds).await?;
            let app = App::new(engine.clone(), config)?;
            let listener = tokio::net::TcpListener::bind(&listen).await?;
            orbit::ops::log(
                "server_started",
                serde_json::json!({"listen":listen,"version":env!("CARGO_PKG_VERSION")}),
            );
            let operations = app.operations.clone();
            let reconciliation_ops = operations.clone();
            let reconciler = tokio::spawn(async move {
                loop {
                    #[cfg(feature = "fault-injection")]
                    if std::env::var_os("ORBIT_TEST_NO_RECONCILE").is_some() {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        continue;
                    }
                    let success = engine.reconcile().await.is_ok();
                    reconciliation_ops.reconciliation(success);
                    if !success {
                        orbit::ops::log("reconciliation_failed", serde_json::json!({}));
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            });
            let (stop, signal) = tokio::sync::watch::channel(false);
            let signals = tokio::spawn(async move {
                let result = orbit::ops::shutdown_signal().await;
                operations.stop();
                orbit::ops::log(
                    "server_draining",
                    serde_json::json!({"signal_listener_ok":result.is_ok()}),
                );
                let _ = stop.send(true);
            });
            use std::future::IntoFuture;
            let serving = axum::serve(listener, api::router(app))
                .with_graceful_shutdown(orbit::ops::stopped(signal.clone()))
                .into_future();
            tokio::pin!(serving);
            tokio::select! {
                result = &mut serving => { result?; },
                _ = orbit::ops::stopped(signal) => {
                    match tokio::time::timeout(Duration::from_secs(shutdown_grace_seconds), &mut serving).await {
                        Ok(result) => result?,
                        Err(_) => orbit::ops::log("server_drain_deadline", serde_json::json!({})),
                    }
                }
            }
            reconciler.abort();
            signals.abort();
            orbit::ops::log("server_stopped", serde_json::json!({}));
            return Ok(());
        }
        Commands::Validate {
            definition,
            base_revision,
            task,
        } => {
            let def = Definition::load_with_overrides(
                &definition,
                base_revision.as_deref(),
                task.as_deref(),
            )?;
            serde_json::json!({"valid": true, "name": def.metadata.name})
        }
        Commands::Run(run_args) => {
            let (definition_path, base_revision, task, scope, request_id, parent_run_id) =
                match run_args.action {
                    Some(RunAction::Submit(args)) => (
                        args.definition,
                        args.base_revision,
                        args.task,
                        args.scope,
                        args.request_id,
                        args.parent_run_id,
                    ),
                    None => {
                        let path = run_args.definition.ok_or_else(|| {
                            anyhow::anyhow!(
                                "definition path required; see 'orbit run submit --help' or 'orbit run --help'"
                            )
                        })?;
                        (
                            path,
                            run_args.base_revision,
                            run_args.task,
                            run_args.scope,
                            run_args.request_id,
                            run_args.parent_run_id,
                        )
                    }
                };
            let definition = Definition::load_with_overrides(
                &definition_path,
                base_revision.as_deref(),
                task.as_deref(),
            )?;
            let request_id = request_id.unwrap_or_else(id);
            eprintln!("submission request_id={request_id}");
            client
                .post(
                    "/runs",
                    &Submit {
                        scope: scope
                            .as_deref()
                            .map(orbit::governance::Scope::parse)
                            .transpose()?,
                        request_id,
                        definition,
                        parent_run_id,
                    },
                )
                .await?
        }
        Commands::Runs { output } => {
            output_format = output.unwrap_or(output_format);
            client.get("/runs").await?
        }
        Commands::Limits => client.get("/limits").await?,
        Commands::Workers => client.get("/workers").await?,
        Commands::Queues => client.get("/queues").await?,
        Commands::SetLimits {
            max_active_roots,
            max_running_attempts,
            max_attempts_per_worker,
        } => {
            let limits = Limits {
                max_active_roots,
                max_running_attempts,
                max_attempts_per_worker,
            };
            limits.validate()?;
            client.post("/limits", &limits).await?
        }
        Commands::Inspect { run_id, json } => {
            let val = client.get(&format!("/runs/{run_id}")).await?;
            if !json && output_format != Output::Jsonl {
                output_format = Output::Text;
            }
            val
        }
        Commands::ExportRun {
            run_id,
            output,
            max_bytes,
        } => orbit::run_export::export(&client, &run_id, &output, max_bytes).await?,
        Commands::Events {
            run_id,
            output,
            after,
            follow,
        } => {
            output_format = output.unwrap_or(output_format);
            if follow {
                use std::io::Write;
                let mut cursor = after.unwrap_or(0);
                loop {
                    let rows = client
                        .get(&format!("/runs/{run_id}/events?after={cursor}"))
                        .await?;
                    for row in rows
                        .as_array()
                        .ok_or_else(|| anyhow::anyhow!("invalid event response"))?
                    {
                        println!("{}", serde_json::to_string(row)?);
                        std::io::stdout().flush()?;
                        cursor = row["sequence"]
                            .as_u64()
                            .ok_or_else(|| anyhow::anyhow!("invalid event sequence"))?;
                    }
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => return Ok(()),
                        _ = tokio::time::sleep(Duration::from_millis(250)) => {}
                    }
                }
            }
            let suffix = after.map(|n| format!("?after={n}")).unwrap_or_default();
            client
                .get(&format!("/runs/{run_id}/events{suffix}"))
                .await?
        }
        Commands::Cancel { run_id } => {
            client
                .post(&format!("/runs/{run_id}/cancel"), &serde_json::json!({}))
                .await?
        }
        Commands::Signal {
            run_id,
            step,
            request_id,
            payload,
        } => {
            let payload = match payload {
                Some(path) => {
                    let bytes = tokio::fs::read(path).await?;
                    anyhow::ensure!(bytes.len() <= 16384, "signal payload exceeds 16384 bytes");
                    serde_json::from_slice(&bytes)?
                }
                None => serde_json::Value::Null,
            };
            let request_id = request_id.unwrap_or_else(id);
            eprintln!("signal request_id={request_id}");
            client
                .post(
                    &format!("/runs/{run_id}/signals"),
                    &Signal {
                        request_id,
                        step,
                        payload,
                    },
                )
                .await?
        }
        Commands::Worker {
            capability,
            workspaces,
            once,
            shutdown_grace_seconds,
            agent_runtime,
            execution_config,
        } => {
            let mut client = client;
            if let Some(path) = execution_config {
                anyhow::ensure!(
                    ["repository.code", "repository.test"].contains(&capability.as_str()),
                    "--execution-config requires a repository worker"
                );
                let config: orbit::execution::WorkerConfig =
                    serde_json::from_slice(&tokio::fs::read(path).await?)?;
                config.validate()?;
                client.execution_config = Some(std::sync::Arc::new(config));
            }
            if let Some(path) = agent_runtime {
                anyhow::ensure!(
                    capability == "agent.run",
                    "--agent-runtime requires agent.run"
                );
                let runtime: orbit::command_agent::CommandAgent =
                    serde_json::from_slice(&tokio::fs::read(path).await?)?;
                runtime.validate()?;
                client.command_agent = Some(std::sync::Arc::new(runtime));
            }
            let (stop, signal) = tokio::sync::watch::channel(false);
            let signals = tokio::spawn(async move {
                let _ = orbit::ops::shutdown_signal().await;
                orbit::ops::log("worker_draining", serde_json::json!({}));
                let _ = stop.send(true);
            });
            let result = worker::run_until(
                client,
                capability,
                workspaces,
                once,
                signal,
                Duration::from_secs(shutdown_grace_seconds),
            )
            .await;
            signals.abort();
            result?;
            return Ok(());
        }
        Commands::Artifact {
            run_id,
            artifact_id,
            output,
        } => {
            let run = client.get(&format!("/runs/{run_id}")).await?;
            let artifact = run["artifacts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["id"] == artifact_id)
                .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
            let artifact = serde_json::from_value(artifact.clone())?;
            let bytes = client.artifact(&run_id, &artifact).await?;
            use tokio::io::AsyncWriteExt;
            let mut file = tokio::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(output)
                .await?;
            file.write_all(&bytes).await?;
            serde_json::json!({"status":"saved","artifact_id":artifact_id})
        }
    };
    match output_format {
        Output::Text if credential_command => println!("{}", serde_json::to_string_pretty(&value)?),
        Output::Text => print!("{}", orbit::telemetry::format_inspect_human(&value)),
        Output::Json => println!("{}", serde_json::to_string_pretty(&value)?),
        Output::Jsonl => {
            if let Some(rows) = value.as_array() {
                for row in rows {
                    println!("{}", serde_json::to_string(row)?);
                }
            } else {
                println!("{}", serde_json::to_string(&value)?);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod durable_catalog_target_tests {
    use super::*;

    #[test]
    fn durable_catalog_connection_target_is_exact_and_errors_do_not_echo_url() {
        let intended = "postgres://orbit:placeholder@127.0.0.1:55442/orbit_control_plane";
        assert!(validate_durable_catalog_url(intended).is_ok());

        for wrong in [
            "postgres://orbit:placeholder@127.0.0.1:55439/orbit_control_plane",
            "postgres://orbit:placeholder@127.0.0.1:55442/orbit_qualification",
            "postgres://orbit:placeholder@localhost:55442/orbit_control_plane",
        ] {
            let error = validate_durable_catalog_url(wrong).unwrap_err().to_string();
            assert!(!error.contains("placeholder"));
        }
    }
}

#[cfg(test)]
mod credential_status_tests {
    use super::{
        CREDENTIAL_STATUS_ALL_MAX, attach_status_health_dimensions, compact_agy_runtime_effects,
        representation_status_reason, safe_representation_state, snapshot_status,
    };
    use orbit::credential_registry::{
        CredentialInspection, CredentialStatus, CredentialView, GenerationView,
        RepresentationState, RepresentationView,
    };

    fn inspection(representations: Vec<RepresentationView>) -> CredentialInspection {
        CredentialInspection {
            credential: CredentialView {
                id: "11111111-1111-4111-8111-111111111111".into(),
                provider: "antigravity".into(),
                reference: "antigravity-test".into(),
                generation: 1,
                endpoint: None,
                auth_type: "oauth-personal".into(),
                secret_backend: "local-private".into(),
                status: CredentialStatus::Enrolled,
                has_secret: true,
                created_at_ms: 1,
                updated_at_ms: 1,
            },
            generations: vec![GenerationView {
                generation: 1,
                secret_backend: "local-private".into(),
                state: "enrolled".into(),
                has_secret: true,
                created_at_ms: 1,
                retired_at_ms: None,
            }],
            representations,
            identity_bindings: vec![],
        }
    }

    fn representation(
        interface: &str,
        state: RepresentationState,
        validation: &str,
    ) -> RepresentationView {
        RepresentationView {
            id: format!("{interface}-id"),
            generation: 1,
            current_generation: true,
            interface: interface.into(),
            auth_type: if interface == "codex" {
                "chatgpt-device-code"
            } else {
                "oauth-personal"
            }
            .into(),
            state,
            validation: validation.into(),
            capabilities: vec![],
            runtime_provenance: None,
            enrollment_stage: (interface == "agy-cli").then(|| "validated".into()),
            has_secret: true,
            last_validated_at_ms: Some(1),
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    #[test]
    fn agy_status_distinguishes_acp_only_from_valid_dual_representation() {
        let acp_only = inspection(vec![representation(
            "acp",
            RepresentationState::Stored,
            "valid",
        )]);
        assert_eq!(safe_representation_state(&acp_only, "acp", 1), "valid");
        assert_eq!(
            safe_representation_state(&acp_only, "agy-cli", 1),
            "missing"
        );

        let dual = inspection(vec![
            representation("acp", RepresentationState::Stored, "valid"),
            representation("agy-cli", RepresentationState::Stored, "valid"),
        ]);
        assert_eq!(safe_representation_state(&dual, "agy-cli", 1), "valid");
    }

    #[test]
    fn status_never_treats_pending_or_old_generation_as_valid() {
        let pending = inspection(vec![representation(
            "codex",
            RepresentationState::Pending,
            "unvalidated",
        )]);
        assert_eq!(safe_representation_state(&pending, "codex", 1), "pending");
        assert_eq!(safe_representation_state(&pending, "codex", 2), "missing");
    }

    #[test]
    fn stored_but_unvalidated_agy_representation_is_not_reported_valid() {
        let mut unvalidated = representation("agy-cli", RepresentationState::Stored, "unvalidated");
        unvalidated.enrollment_stage = Some("secret_persisted".into());
        unvalidated.last_validated_at_ms = None;
        let inspection = inspection(vec![unvalidated]);
        assert_eq!(
            safe_representation_state(&inspection, "agy-cli", 1),
            "unvalidated"
        );
        let reason = representation_status_reason(&inspection, "agy-cli", 1, "antigravity-test");
        assert!(reason.contains("secret_persisted"));
        assert!(reason.contains("add-representation antigravity-test --interface agy-cli"));
        assert!(!reason.contains("credential://"));
    }

    #[test]
    fn normal_agy_status_effects_are_bounded_and_do_not_echo_file_inventory() {
        let summary = compact_agy_runtime_effects(&[
            serde_json::json!({"path":".gemini/antigravity-cli/antigravity-oauth-token"}),
            serde_json::json!({"path":".gemini/antigravity-cli/cache/index"}),
            serde_json::json!({"path":".gemini/antigravity-cli/builtin"}),
            serde_json::json!({"path":".gemini/antigravity-cli/cli.log"}),
            serde_json::json!({"path":".gemini/antigravity-cli/conversation_summaries.db"}),
        ]);
        assert_eq!(summary["staged_auth_artifact"], true);
        assert_eq!(summary["observed_runtime_entries"], 4);
        assert_eq!(summary["runtime_categories"].as_array().unwrap().len(), 4);
        let rendered = summary.to_string();
        assert!(!rendered.contains(".gemini"));
        assert!(!rendered.contains("antigravity-oauth-token"));
        assert!(!rendered.contains("conversation_summaries.db"));
    }

    #[test]
    fn health_report_separates_auth_runtime_freshness_quota_and_availability() {
        let mut report = serde_json::json!({
            "representations":{"codex":{"validation":"valid"}},
            "status":{"state":"observed","authenticated":true,"provider_scope":"unconfirmed"},
            "availability":{
                "state":"unknown","fresh":true,"observed_at_ms":10,"expires_at_ms":20,
                "quota_buckets":[],"quota_windows":[],"quota_groups":[]
            }
        });
        attach_status_health_dimensions(&mut report);
        assert_eq!(report["health"]["runtime"]["state"], "healthy");
        assert_eq!(report["health"]["provider_scope"]["state"], "unconfirmed");
        assert_eq!(
            report["health"]["status_observation"]["last_snapshot_fresh"],
            true
        );
        assert_eq!(
            report["health"]["scheduling_availability"]["state"],
            "unknown"
        );
        assert_eq!(
            report["health"]["quota_evidence"]["persisted_bucket_count"],
            0
        );
    }

    #[test]
    fn status_all_has_a_fixed_preflight_bound() {
        assert_eq!(CREDENTIAL_STATUS_ALL_MAX, 32);
    }

    #[test]
    fn expired_status_evidence_is_reported_unknown_without_erasing_history() {
        let snapshot = orbit::availability::AvailabilitySnapshot {
            applies_to: orbit::availability::AvailabilityScope::Credential(
                orbit::availability::CredentialIdentity {
                    provider: "antigravity".into(),
                    reference: "antigravity-test".into(),
                    generation: "1".into(),
                    catalog_id: Some("11111111-1111-4111-8111-111111111111".into()),
                },
            ),
            observed_at_ms: 1,
            expires_at_ms: 2,
            state: orbit::availability::AvailabilityState::Ready,
            quota_windows: vec![],
            quota_buckets: vec![],
            quota_groups: vec![],
            source: orbit::availability::EvidenceSource::ProviderNativeStatus,
            confidence: orbit::availability::EvidenceConfidence::AuthoritativeNative,
            source_revision: "status-v1".into(),
            evidence_digest: format!("sha256:{}", "a".repeat(64)),
            provider_observed_at_ms: None,
            provider_status_observation: None,
        };
        let status = snapshot_status(Some(&snapshot), 3);
        assert_eq!(status["state"], "unknown");
        assert_eq!(status["recorded_state"], "ready");
        assert_eq!(status["fresh"], false);
    }
}
