use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use orbit::{
    api::{self, App, Config, Submit},
    availability::AvailabilityStore,
    credential_registry::CredentialStore,
    engine::Engine,
    model::{Definition, Limits, Signal, id},
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
    /// Import one operator-qualified agy token and validate it from a fresh isolated process.
    AddRepresentation {
        reference: String,
        #[arg(long, required = true)]
        interface: String,
        #[arg(long, default_value = "oauth-personal")]
        auth_type: String,
        /// Qualification-only source import; the path is never echoed or persisted.
        #[arg(long, hide = true, required = true)]
        source_file: PathBuf,
        #[arg(long, env = "ORBIT_DATABASE_URL_FILE", hide_env_values = true)]
        database_url_file: Option<PathBuf>,
    },
    /// Capture one Antigravity agy `/usage` response without retaining raw values.
    CaptureAgyUsage {
        reference: String,
        #[arg(long, env = "ORBIT_DATABASE_URL_FILE", hide_env_values = true)]
        database_url_file: Option<PathBuf>,
    },
    /// Run one catalog-backed, non-inference Codex account/status observation.
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
    let home = std::env::var_os("HOME").context("operator HOME unavailable")?;
    let home = PathBuf::from(home).canonicalize()?;
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
            "only the qualified agy-cli oauth-personal representation is enabled"
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
        let result = orbit::agy_cli_representation::import_and_validate(
            &engine.pool,
            &backend,
            reference,
            source_file,
        )
        .await?;
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
            CredentialAction::AddRepresentation { .. } => {
                unreachable!("local representation import handled before API credential resolution")
            }
            CredentialAction::CaptureAgyUsage { .. } => {
                unreachable!("local agy status capture handled before API credential resolution")
            }
            CredentialAction::ProbeCodexStatus { .. } => {
                unreachable!("local Codex status probe handled before API credential resolution")
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
