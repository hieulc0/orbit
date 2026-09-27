use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
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

#[derive(Args)]
struct VerificationArgs {
    #[command(subcommand)]
    action: VerificationAction,
}

#[derive(Subcommand)]
enum VerificationAction {
    /// Execute a verification plan against a workspace.
    Run {
        attempt_id: String,
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long)]
        plan: PathBuf,
        /// Optional container image for isolated execution (e.g. docker.io/library/rust:latest)
        #[arg(long)]
        image: Option<String>,
        /// Optional path to verification policy JSON file
        #[arg(long)]
        policy: Option<PathBuf>,
        #[arg(long, env = "ORBIT_DATABASE_URL_FILE", hide_env_values = true)]
        database_url_file: Option<PathBuf>,
    },
    /// Show a verification run and its step results.
    Show {
        run_id: String,
        #[arg(long, env = "ORBIT_DATABASE_URL_FILE", hide_env_values = true)]
        database_url_file: Option<PathBuf>,
    },
    /// List verification runs for an attempt.
    List {
        attempt_id: String,
        #[arg(long, env = "ORBIT_DATABASE_URL_FILE", hide_env_values = true)]
        database_url_file: Option<PathBuf>,
    },
}

#[derive(Args)]
struct WorkflowArgs {
    #[command(subcommand)]
    action: WorkflowAction,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
enum WorkflowAction {
    /// Run Orbit's fixed live CLI qualification against a disposable temporary Git repository.
    QualifyLive {
        /// Clean disposable Git repository under temp with committed README.md and fixed test.sh.
        #[arg(long, value_name = "PATH", required = true)]
        repo: PathBuf,
        #[arg(long, env = "ORBIT_DATABASE_URL_FILE", hide_env_values = true)]
        database_url_file: Option<PathBuf>,
    },
    /// Start a new workflow run for a task and attempt.
    Start {
        #[arg(value_name = "TASK_ID")]
        pos_task_id: Option<String>,
        #[arg(value_name = "ATTEMPT_ID")]
        pos_attempt_id: Option<String>,
        #[arg(long)]
        task_id: Option<String>,
        #[arg(long)]
        attempt_id: Option<String>,
        /// Inline task description / prompt
        #[arg(long)]
        task: Option<String>,
        /// Path to file containing task prompt
        #[arg(long)]
        task_file: Option<PathBuf>,
        /// Target repository path (default: current directory ".")
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Base git revision (default: "HEAD")
        #[arg(long)]
        base_revision: Option<String>,
        #[arg(long, default_value = "software-change")]
        kind: String,
        #[arg(long, default_value = "3")]
        max_iterations: u32,
        /// Optional path to verification policy file
        #[arg(long)]
        policy: Option<PathBuf>,
        /// Optional path to regression policy file
        #[arg(long)]
        regression_policy: Option<PathBuf>,
        /// Optional path to selection policy file
        #[arg(long)]
        selection_policy: Option<PathBuf>,
        /// Detach execution to background instead of waiting for completion
        #[arg(long)]
        detach: bool,
        #[arg(long, env = "ORBIT_DATABASE_URL_FILE", hide_env_values = true)]
        database_url_file: Option<PathBuf>,
    },
    /// Execute or resume an existing workflow run to completion.
    Run {
        workflow_run_id: String,
        #[arg(long, env = "ORBIT_DATABASE_URL_FILE", hide_env_values = true)]
        database_url_file: Option<PathBuf>,
    },
    /// Show a workflow run and its role execution stages.
    Show {
        workflow_run_id: String,
        #[arg(long, env = "ORBIT_DATABASE_URL_FILE", hide_env_values = true)]
        database_url_file: Option<PathBuf>,
    },
    /// List workflow runs, optionally filtered by attempt.
    List {
        #[arg(long)]
        attempt: Option<String>,
        #[arg(long, env = "ORBIT_DATABASE_URL_FILE", hide_env_values = true)]
        database_url_file: Option<PathBuf>,
    },
    /// Cancel an active workflow run.
    Cancel {
        workflow_run_id: String,
        #[arg(long, default_value = "cancelled by operator")]
        reason: String,
        #[arg(long, env = "ORBIT_DATABASE_URL_FILE", hide_env_values = true)]
        database_url_file: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum Commands {
    /// Operator credential registry and enrollment.
    Credential(CredentialArgs),
    /// Execute or inspect isolated verification evidence.
    Verification(VerificationArgs),
    /// Manage and inspect verified multi-role workflow runs.
    Workflow(WorkflowArgs),
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

fn ensure_cli_workflow_execution_enabled(action: &WorkflowAction) -> Result<()> {
    if matches!(
        action,
        WorkflowAction::Start { .. } | WorkflowAction::Run { .. }
    ) {
        anyhow::bail!(
            "CLI_WORKFLOW_EXECUTION_GATED: workflow start and resume remain gated pending qualification"
        );
    }
    Ok(())
}

const LIVE_QUALIFICATION_VERIFICATION_IMAGE: &str = "docker.io/library/alpine@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b";
const LIVE_QUALIFICATION_README_CONTENT: &str = "ORBIT_CLI_WORKFLOW_FIXTURE_READY";
const LIVE_QUALIFICATION_TEST_MARKER: &str = "ORBIT_CLI_WORKFLOW_CHECK_PASSED";
const LIVE_QUALIFICATION_TEST_SCRIPT: &str = "#!/bin/sh\nset -eu\nif ! printf '%s\\n' 'ORBIT_CLI_WORKFLOW_FIXTURE_READY' | cmp -s - README.md; then\n    printf '%s\\n' 'Live qualification candidate contract failed' >&2\n    exit 1\nfi\nprintf '%s\\n' 'ORBIT_CLI_WORKFLOW_CHECK_PASSED'\n";

struct LiveCliQualificationPolicies {
    verification: orbit::verification::VerificationPolicy,
    regression: orbit::regression_strategy::RegressionPolicy,
    selection: orbit::regression_strategy::SelectionPolicy,
}

fn live_cli_qualification_policies() -> LiveCliQualificationPolicies {
    use orbit::{
        regression_strategy::{
            RegressionFallbackBehavior, RegressionPolicy, SelectionPolicy, VerificationCheck,
            VerificationTier,
        },
        verification::{AllowedCommand, VerificationPolicy},
    };

    let mut verification = VerificationPolicy::new(
        "orbit-live-cli-verification-v1",
        "Fixed Orbit live CLI qualification check",
    );
    verification.required_steps = vec!["candidate-contract".into()];
    verification.allowed_commands = vec![AllowedCommand::with_prefix("sh", vec!["test.sh".into()])];
    verification.network_policy = orbit::verification::VerificationNetworkPolicy::None;
    verification.cache_policy = orbit::verification::VerificationCachePolicy::Clean;

    let mut selection = SelectionPolicy::new(
        "orbit-live-cli-selection-v1",
        "Fixed Orbit live CLI qualification selection",
    );
    let mut check = VerificationCheck::new_command(
        "candidate-contract",
        "Check the live qualification candidate",
        vec![
            VerificationTier::Fast,
            VerificationTier::Standard,
            VerificationTier::Full,
        ],
        vec!["sh".into(), "test.sh".into()],
    );
    check.always_run = true;
    selection.checks.push(check);

    let mut regression = RegressionPolicy::new(
        "orbit-live-cli-regression-v1",
        "Fixed Orbit live CLI qualification regression",
    );
    regression.fallback_behavior = RegressionFallbackBehavior::FailClosed;
    regression.selection_policy_id = Some(selection.id.clone());
    regression.selection_policy_version = Some(selection.version);
    regression.selection_policy_digest = Some(selection.digest());

    LiveCliQualificationPolicies {
        verification,
        regression,
        selection,
    }
}

struct LiveQualificationRepository {
    path: PathBuf,
    base_revision: String,
}

const LIVE_QUALIFICATION_LOCAL_GIT_CONFIG_ALLOWLIST: &[&str] = &[
    "core.repositoryformatversion",
    "core.filemode",
    "core.bare",
    "core.logallrefupdates",
    "user.name",
    "user.email",
];

fn local_git_config_keys_for_qualification(repository: &Path) -> Result<Vec<String>> {
    let keys = git_readonly_output(
        repository,
        &[
            "config",
            "--local",
            "--no-includes",
            "--null",
            "--name-only",
            "--list",
        ],
    )?;
    Ok(keys
        .split('\0')
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
        .collect())
}

fn validate_local_git_config_for_qualification(repository: &Path) -> Result<()> {
    let keys = local_git_config_keys_for_qualification(repository)?;
    for key in &keys {
        anyhow::ensure!(
            LIVE_QUALIFICATION_LOCAL_GIT_CONFIG_ALLOWLIST.contains(&key.as_str()),
            "qualification repository Git configuration contains an unsupported setting"
        );
    }
    anyhow::ensure!(
        keys.iter().any(|key| key == "core.repositoryformatversion")
            && keys.iter().any(|key| key == "core.bare"),
        "qualification repository Git configuration is incomplete"
    );
    Ok(())
}

fn validate_live_qualification_repository(path: &Path) -> Result<LiveQualificationRepository> {
    let canonical = path
        .canonicalize()
        .context("qualification repository path must exist")?;
    anyhow::ensure!(
        canonical.is_dir(),
        "qualification repository must be a directory"
    );
    let temp_root = std::env::temp_dir()
        .canonicalize()
        .context("operating system temp directory is unavailable")?;
    anyhow::ensure!(
        canonical != temp_root && canonical.starts_with(&temp_root),
        "qualification repository must be under the operating system temp directory"
    );
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .canonicalize()
        .context("Orbit source checkout path is unavailable")?;
    anyhow::ensure!(
        !canonical.starts_with(&source_root) && !source_root.starts_with(&canonical),
        "qualification repository cannot be the Orbit source checkout or its parent"
    );

    let mut has_git_directory = false;
    let mut has_readme = false;
    let mut has_test_script = false;
    for entry in fs::read_dir(&canonical)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        match entry.file_name().to_str() {
            Some(".git") => {
                anyhow::ensure!(
                    file_type.is_dir(),
                    "qualification repository must be a standalone Git repository"
                );
                has_git_directory = true;
            }
            Some("README.md") => {
                anyhow::ensure!(
                    file_type.is_file(),
                    "qualification repository README.md must be a regular file"
                );
                anyhow::ensure!(
                    fs::metadata(entry.path())?.len() <= 4096,
                    "qualification repository README.md exceeds 4 KiB"
                );
                let readme = fs::read_to_string(entry.path())
                    .context("qualification repository README.md must be UTF-8 text")?;
                anyhow::ensure!(
                    readme.trim() != LIVE_QUALIFICATION_README_CONTENT,
                    "qualification repository README.md must require a candidate mutation"
                );
                has_readme = true;
            }
            Some("test.sh") => {
                anyhow::ensure!(
                    file_type.is_file()
                        && fs::read(entry.path())? == LIVE_QUALIFICATION_TEST_SCRIPT.as_bytes(),
                    "qualification repository must contain the fixed contract test.sh"
                );
                has_test_script = true;
            }
            _ => anyhow::bail!(
                "qualification repository may contain only .git, README.md and the fixed test.sh before the run"
            ),
        }
    }
    anyhow::ensure!(
        has_git_directory && has_readme && has_test_script,
        "qualification repository must contain a Git directory, README.md and fixed test.sh"
    );

    let hooks_path = canonical.join(".git/hooks");
    let hooks_metadata = fs::symlink_metadata(&hooks_path)
        .context("qualification repository Git hooks directory is unavailable")?;
    anyhow::ensure!(
        hooks_metadata.is_dir() && !hooks_metadata.file_type().is_symlink(),
        "qualification repository Git hooks directory must be local"
    );
    for entry in fs::read_dir(&hooks_path)? {
        let entry = entry?;
        let name = entry.file_name();
        let metadata = fs::symlink_metadata(entry.path())?;
        anyhow::ensure!(
            name.to_str().is_some_and(|name| name.ends_with(".sample"))
                && metadata.is_file()
                && !metadata.file_type().is_symlink(),
            "qualification repository must not contain active or custom Git hooks"
        );
    }
    validate_local_git_config_for_qualification(&canonical)?;

    let top_level = git_readonly_output(&canonical, &["rev-parse", "--show-toplevel"])?;
    anyhow::ensure!(
        Path::new(&top_level).canonicalize()? == canonical,
        "qualification path must be the Git repository root"
    );
    let tracked_files = git_readonly_output(&canonical, &["ls-files", "-z"])?;
    let tracked_files: Vec<&str> = tracked_files
        .split('\0')
        .filter(|path| !path.is_empty())
        .collect();
    anyhow::ensure!(
        tracked_files == ["README.md", "test.sh"],
        "qualification repository must track README.md and the fixed test.sh"
    );
    anyhow::ensure!(
        git_readonly_output(
            &canonical,
            &["status", "--porcelain=v1", "--untracked-files=all"]
        )?
        .is_empty(),
        "qualification repository must have a clean worktree"
    );
    let base_revision = git_readonly_output(&canonical, &["rev-parse", "--verify", "HEAD"])?;
    anyhow::ensure!(
        (base_revision.len() == 40 || base_revision.len() == 64)
            && base_revision.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "qualification repository must have a committed Git baseline"
    );

    Ok(LiveQualificationRepository {
        path: canonical,
        base_revision,
    })
}

fn git_readonly_output(repository: &Path, args: &[&str]) -> Result<String> {
    let output = read_only_qualification_git_command(repository, args)
        .output()
        .context("could not start Git while validating the qualification repository")?;
    anyhow::ensure!(
        output.status.success(),
        "qualification repository failed Git validation"
    );
    String::from_utf8(output.stdout)
        .context("qualification repository Git metadata was not UTF-8")
        .map(|value| value.trim().to_owned())
}

fn read_only_qualification_git_command(repository: &Path, args: &[&str]) -> std::process::Command {
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into());
    let mut command = std::process::Command::new("git");
    command
        .env_clear()
        .env("PATH", path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_COUNT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EXTERNAL_DIFF", "")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .args([
            "--no-pager",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "diff.external=",
            "-c",
            "core.pager=cat",
        ])
        .arg("-C")
        .arg(repository);
    if args.first() == Some(&"diff") {
        command.args(["diff", "--no-ext-diff", "--no-textconv"]);
        command.args(&args[1..]);
    } else {
        command.args(args);
    }
    command
}

fn qualification_environment_from_image_id(
    image_id: &str,
) -> Result<orbit::verification::EnvironmentIdentity> {
    let image_id = image_id
        .trim()
        .strip_prefix("sha256:")
        .unwrap_or(image_id.trim());
    anyhow::ensure!(
        image_id.len() == 64 && image_id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "local Podman verification image identity is invalid"
    );
    anyhow::ensure!(
        std::env::consts::OS == "linux",
        "live CLI qualification requires Linux rootless Podman"
    );
    Ok(orbit::verification::EnvironmentIdentity {
        execution_profile: "sandboxed-container".into(),
        isolation: "rootless-podman".into(),
        runtime_image: Some(LIVE_QUALIFICATION_VERIFICATION_IMAGE.into()),
        runtime_image_digest: Some(format!("sha256:{image_id}")),
        oci_runtime: Some("podman".into()),
        network_policy: orbit::verification::VerificationNetworkPolicy::None,
        cache_policy: orbit::verification::VerificationCachePolicy::Clean,
        environment_policy_digest: None,
        integration_environment_digest: None,
        browser_verification_digest: None,
        browser_runtime_image_digest: None,
        regression_policy_digest: None,
        selection_digest: None,
        architecture: std::env::consts::ARCH.into(),
        os: std::env::consts::OS.into(),
        orbit_version: env!("CARGO_PKG_VERSION").into(),
    })
}

fn reset_aware_selection_evidence(reason: &str) -> serde_json::Value {
    let fields: std::collections::BTreeMap<&str, &str> = reason
        .split("; ")
        .filter_map(|field| field.split_once('='))
        .collect();
    let rank = reason
        .strip_prefix("reset-aware rank=")
        .and_then(|tail| tail.split_once(';'))
        .map(|(rank, _)| rank.trim())
        .filter(|rank| rank.bytes().all(|byte| byte.is_ascii_digit()));
    let weekly_reset_rank = match reason.split("; ").nth(1) {
        Some("known_weekly_reset") => Some("known_weekly_reset"),
        Some("weekly_reset_unknown_or_not_applicable") => {
            Some("weekly_reset_unknown_or_not_applicable")
        }
        _ => None,
    };
    let quota_snapshot_freshness = fields
        .get("quota_snapshot_freshness")
        .filter(|value| matches!(**value, "FRESH" | "STALE" | "ABSENT"));
    let availability = fields.get("availability").filter(|value| {
        matches!(
            **value,
            "Ready"
                | "Limited"
                | "Cooldown"
                | "RateLimited"
                | "QuotaExhausted"
                | "AuthFailed"
                | "RuntimeUnavailable"
                | "CapabilityMismatch"
                | "Unknown"
        )
    });
    let five_hour_remaining = fields
        .get("5h_remaining")
        .copied()
        .filter(|value| is_safe_quota_percent(value));
    let seven_day_remaining = fields
        .get("7d_remaining")
        .copied()
        .filter(|value| is_safe_quota_percent(value));
    let seven_day_reset_at_ms = fields
        .get("7d_reset_at_ms")
        .filter(|value| **value == "unknown" || value.bytes().all(|byte| byte.is_ascii_digit()));
    let provider_preference_rank = fields
        .get("provider_preference_rank")
        .filter(|value| value.bytes().all(|byte| byte.is_ascii_digit()));
    let tie_break = fields
        .get("tie_break")
        .filter(|value| **value == "provider_preference_then_stable_account_id");
    let rejected_summary_items = reason
        .split_once("; rejected=")
        .and_then(|(_, summary)| summary.strip_prefix('['))
        .and_then(|summary| summary.strip_suffix(']'))
        .map(|summary| {
            if summary.is_empty() {
                0
            } else {
                summary
                    .matches("codex:")
                    .count()
                    .saturating_add(summary.matches("antigravity:").count())
                    .min(16)
            }
        });
    serde_json::json!({
        "ranking": if reason.starts_with("reset-aware rank=") { "reset-aware" } else { "other_or_unknown" },
        "rank": rank,
        "weekly_reset_rank": weekly_reset_rank,
        "quota_snapshot_freshness": quota_snapshot_freshness,
        "availability": availability,
        "five_hour_remaining": five_hour_remaining,
        "seven_day_remaining": seven_day_remaining,
        "seven_day_reset_at_ms": seven_day_reset_at_ms,
        "five_hour_reset_at_ms": null,
        "provider_preference_rank": provider_preference_rank,
        "tie_break": tie_break,
        "rejected_candidate_summary_items": rejected_summary_items,
        "selection_reason": {
            "ranking": if reason.starts_with("reset-aware rank=") { "reset-aware" } else { "other_or_unknown" },
            "rank": rank,
            "weekly_reset_rank": weekly_reset_rank,
            "quota_snapshot_freshness": quota_snapshot_freshness,
            "availability": availability,
            "five_hour_remaining": five_hour_remaining,
            "seven_day_remaining": seven_day_remaining,
            "seven_day_reset_at_ms": seven_day_reset_at_ms,
            "provider_preference_rank": provider_preference_rank,
            "tie_break": tie_break
        }
    })
}

fn is_safe_quota_percent(value: &str) -> bool {
    value == "unknown"
        || (!value.is_empty()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'.' | b'%')))
}

fn safe_quota_percent(value: Option<f64>) -> Option<f64> {
    value.filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
}

fn bounded_tool_counts(value: &serde_json::Value) -> serde_json::Value {
    const SAFE_TOOL_NAMES: &[&str] = &[
        "read_file",
        "write_file",
        "edit_file",
        "list_directory",
        "find_path",
        "create_directory",
        "move",
        "copy",
        "delete_file",
        "delete_directory",
        "grep",
        "shell",
        "terminal/output",
        "terminal/wait_for_exit",
        "terminal/kill",
        "terminal/release",
        "git_status",
        "git_diff",
        "git_show",
        "fs.read_text_file",
        "fs.write_text_file",
        "fs.edit_file",
        "fs.list_directory",
        "fs.find_path",
        "fs.create_directory",
        "fs.move",
        "fs.copy",
        "fs.delete_file",
        "fs.delete_directory",
        "search.grep",
        "terminal.create",
        "terminal.output",
        "terminal.wait_for_exit",
        "terminal.kill",
        "terminal.release",
        "git.status",
        "git.diff",
        "git.show",
    ];
    let Some(counts) = value.as_object() else {
        return serde_json::json!({});
    };
    let bounded = counts
        .iter()
        .filter(|(name, _)| SAFE_TOOL_NAMES.contains(&name.as_str()))
        .filter_map(|(name, count)| {
            count
                .as_u64()
                .map(|count| (name.clone(), serde_json::Value::from(count)))
        })
        .take(SAFE_TOOL_NAMES.len())
        .collect::<serde_json::Map<_, _>>();
    serde_json::Value::Object(bounded)
}

fn tool_count(counts: &serde_json::Value, names: &[&str]) -> u64 {
    names
        .iter()
        .filter_map(|name| counts.get(*name).and_then(serde_json::Value::as_u64))
        .fold(0, u64::saturating_add)
}

async fn selected_credential_quota_evidence(
    pool: &sqlx::PgPool,
    target: &orbit::workflow::ResolvedExecutionTarget,
) -> Result<serde_json::Value> {
    let Some(reference) = target.credential_id.as_deref() else {
        return Ok(serde_json::json!({"state":"unknown", "quota_windows":[]}));
    };
    let credential = orbit::credential_registry::CredentialStore::new(pool)
        .get(reference)
        .await?;
    let Some(credential) = credential else {
        return Ok(serde_json::json!({"state":"unknown", "quota_windows":[]}));
    };
    if credential.provider != target.provider
        || target.credential_generation != u32::try_from(credential.generation).ok()
    {
        return Ok(serde_json::json!({
            "state":"identity_or_generation_mismatch",
            "five_hour_reset_at_ms":null,
            "quota_windows":[]
        }));
    }
    let snapshot = orbit::availability::AvailabilityStore::new(pool)
        .current_for_credential(&credential.identity())
        .await?;
    let Some(snapshot) = snapshot else {
        return Ok(serde_json::json!({"state":"unknown", "quota_windows":[]}));
    };

    let now_ms = unix_time_ms()?;
    let fresh = snapshot.observed_at_ms <= now_ms && now_ms < snapshot.expires_at_ms;
    let mut windows = Vec::new();
    let mut append_window = |duration_minutes: Option<i64>,
                             remaining_percent: Option<f64>,
                             resets_at_ms: Option<i64>,
                             exhausted: Option<bool>| {
        if windows.len() >= 64 {
            return;
        }
        let duration_minutes = duration_minutes.filter(|minutes| (0..=525_600).contains(minutes));
        let window_kind = match duration_minutes {
            Some(300) => "five_hour",
            Some(10_080) => "seven_day",
            Some(_) => "other_duration",
            None => "unknown_duration",
        };
        windows.push(serde_json::json!({
            "window_kind": window_kind,
            "duration_minutes": duration_minutes,
            "remaining_percent": safe_quota_percent(remaining_percent),
            "resets_at_ms": resets_at_ms.filter(|reset| *reset >= 0),
            "exhausted": exhausted
        }));
    };
    for window in snapshot.quota_windows.iter().take(32) {
        append_window(
            window.duration_minutes,
            window
                .remaining_percent
                .or_else(|| window.used_percent.map(|used| 100.0 - used)),
            window.resets_at_ms,
            window.exhausted,
        );
    }
    for bucket in snapshot.quota_buckets.iter().take(16) {
        for window in bucket.windows.iter().take(16) {
            append_window(
                window.duration_minutes,
                window
                    .remaining_percent
                    .or_else(|| window.remaining_fraction.map(|fraction| fraction * 100.0))
                    .or_else(|| window.used_percent.map(|used| 100.0 - used)),
                window.resets_at_ms,
                window.exhausted,
            );
        }
    }
    Ok(serde_json::json!({
        "state": if fresh { "fresh" } else { "stale" },
        "availability": snapshot.state,
        "observed_at_ms": snapshot.observed_at_ms,
        "expires_at_ms": snapshot.expires_at_ms,
        "five_hour_reset_at_ms": null,
        "five_hour_reset_attribution": "unknown_without_exact_selected_model_group",
        "quota_windows": windows
    }))
}

fn selection_contains_required_check(
    selection: Option<&orbit::regression_strategy::VerificationSelection>,
    workspace_state_id: &str,
    tier: orbit::regression_strategy::VerificationTier,
) -> bool {
    selection.is_some_and(|selection| {
        selection.workspace_state_id == workspace_state_id
            && selection.requested_tier == tier
            && selection
                .selected_checks
                .iter()
                .any(|check| check.check_id == "candidate-contract")
    })
}

fn executed_fixed_check_passed(
    run: Option<&orbit::verification::VerificationRun>,
    expected_environment: &orbit::verification::EnvironmentIdentity,
) -> bool {
    let Some(run) = run else { return false };
    let check = run
        .step_runs
        .iter()
        .find(|step| step.step_id == "candidate-contract");
    let planned_command = run.plan_snapshot.steps.iter().any(|step| {
        step.id == "candidate-contract"
            && step.argv == ["sh".to_owned(), "test.sh".to_owned()]
            && step.required
    });
    run.overall_result == Some(orbit::verification::VerificationRunResult::Passed)
        && check.is_some_and(|step| {
            step.status == orbit::verification::VerificationStepStatus::Passed
                && step.exit_code == Some(0)
                && step
                    .stdout_preview
                    .as_deref()
                    .is_some_and(|stdout| stdout.contains(LIVE_QUALIFICATION_TEST_MARKER))
        })
        && planned_command
        && run.environment_identity.execution_profile == expected_environment.execution_profile
        && run.environment_identity.isolation == expected_environment.isolation
        && run.environment_identity.runtime_image == expected_environment.runtime_image
        && run.environment_identity.runtime_image_digest
            == expected_environment.runtime_image_digest
        && run.environment_identity.oci_runtime == expected_environment.oci_runtime
        && run.environment_identity.network_policy == expected_environment.network_policy
        && run.environment_identity.cache_policy == expected_environment.cache_policy
}

fn candidate_contract_evidence(repository: &LiveQualificationRepository) -> serde_json::Value {
    let expected_readme = format!("{LIVE_QUALIFICATION_README_CONTENT}\n");
    let readme_is_exact = fs::read_to_string(repository.path.join("README.md"))
        .is_ok_and(|readme| readme == expected_readme);
    let fixed_harness_unchanged = fs::read(repository.path.join("test.sh"))
        .is_ok_and(|script| script == LIVE_QUALIFICATION_TEST_SCRIPT.as_bytes());
    let changed_tracked_files = git_readonly_output(
        &repository.path,
        &[
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            &repository.base_revision,
            "--",
        ],
    )
    .ok()
    .map(|paths| {
        paths
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>()
    })
    .unwrap_or_default();
    let untracked_file_count = git_readonly_output(
        &repository.path,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )
    .ok()
    .map(|paths| paths.split('\0').filter(|path| !path.is_empty()).count());
    let repository_shape_valid = fs::read_dir(&repository.path).is_ok_and(|entries| {
        entries.into_iter().all(|entry| {
            let Ok(entry) = entry else { return false };
            let name = entry.file_name();
            let Ok(file_type) = entry.file_type() else {
                return false;
            };
            matches!(name.to_str(), Some(".git") if file_type.is_dir())
                || matches!(name.to_str(), Some("README.md" | "test.sh") if file_type.is_file())
        })
    });
    let only_expected_readme_change = changed_tracked_files == ["README.md"]
        && untracked_file_count == Some(0)
        && repository_shape_valid
        && readme_is_exact
        && fixed_harness_unchanged;
    serde_json::json!({
        "readme_is_exact": readme_is_exact,
        "fixed_harness_unchanged": fixed_harness_unchanged,
        "changed_tracked_files": changed_tracked_files,
        "untracked_file_count": untracked_file_count,
        "repository_shape_valid": repository_shape_valid,
        "only_expected_readme_change": only_expected_readme_change
    })
}

async fn pinned_qualification_verification_environment()
-> Result<orbit::verification::EnvironmentIdentity> {
    anyhow::ensure!(
        unsafe { libc::geteuid() } != 0,
        "live CLI qualification requires rootless Podman"
    );
    let rootless = tokio::process::Command::new("podman")
        .args([
            "--remote=false",
            "info",
            "--format",
            "{{.Host.Security.Rootless}}",
        ])
        .output()
        .await
        .context("could not inspect the local rootless Podman runtime")?;
    anyhow::ensure!(
        rootless.status.success() && String::from_utf8_lossy(&rootless.stdout).trim() == "true",
        "live CLI qualification requires a working rootless Podman runtime"
    );
    let image = tokio::process::Command::new("podman")
        .args([
            "--remote=false",
            "image",
            "inspect",
            LIVE_QUALIFICATION_VERIFICATION_IMAGE,
            "--format",
            "{{.Id}}",
        ])
        .output()
        .await
        .context("could not inspect the locally pinned live qualification image")?;
    anyhow::ensure!(
        image.status.success(),
        "locally pinned live qualification image is unavailable in rootless Podman"
    );
    let image_id = String::from_utf8(image.stdout)
        .context("Podman returned a non-UTF-8 verification image identity")?;
    qualification_environment_from_image_id(&image_id)
}

fn durable_tool_call_audit_is_strict(
    audit: &serde_json::Value,
    tool_call_count: i64,
    tool_success_count: i64,
    tool_failure_count: i64,
) -> bool {
    let (Ok(expected_total), Ok(expected_success), Ok(expected_failure)) = (
        u64::try_from(tool_call_count),
        u64::try_from(tool_success_count),
        u64::try_from(tool_failure_count),
    ) else {
        return false;
    };
    if expected_failure != 0 || expected_success != expected_total {
        return false;
    }
    let Some(summary) = audit.get("summary") else {
        return false;
    };
    let Some(entries) = audit.get("entries").and_then(serde_json::Value::as_array) else {
        return false;
    };
    let Some(provider_updates) = audit
        .get("provider_updates")
        .and_then(serde_json::Value::as_array)
    else {
        return false;
    };
    let common_evidence_is_clean = audit
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        == Some(2)
        && summary.get("total").and_then(serde_json::Value::as_u64) == Some(expected_total)
        && summary
            .get("callback_count")
            .and_then(serde_json::Value::as_u64)
            == Some(expected_total)
        && summary
            .get("provider_notification_count")
            .and_then(serde_json::Value::as_u64)
            == Some(expected_total)
        && summary
            .get("successful")
            .and_then(serde_json::Value::as_u64)
            == Some(expected_success)
        && summary
            .get("unsuccessful")
            .and_then(serde_json::Value::as_u64)
            == Some(expected_failure)
        && summary
            .get("unmatched_provider_calls")
            .and_then(serde_json::Value::as_u64)
            == Some(0)
        && summary
            .get("unmatched_callbacks")
            .and_then(serde_json::Value::as_u64)
            == Some(0)
        && summary.get("denied").and_then(serde_json::Value::as_u64) == Some(0)
        && summary
            .get("mutating_unknown")
            .and_then(serde_json::Value::as_u64)
            == Some(0)
        && audit
            .get("omitted_count")
            .and_then(serde_json::Value::as_u64)
            == Some(0)
        && audit
            .get("provider_tool_names_omitted")
            .and_then(serde_json::Value::as_u64)
            == Some(0);
    if !common_evidence_is_clean {
        return false;
    }

    if expected_total == 0 {
        return audit
            .get("correlation_capability")
            .and_then(serde_json::Value::as_str)
            == Some("NOT_EXERCISED")
            && summary.get("mutating").and_then(serde_json::Value::as_u64) == Some(0)
            && entries.is_empty()
            && provider_updates.is_empty();
    }

    audit
        .get("correlation_capability")
        .and_then(serde_json::Value::as_str)
        == Some("SUPPORTED")
        && usize::try_from(expected_total).ok() == Some(entries.len())
        && successful_tool_audit_rows_are_correlated(
            entries,
            provider_updates,
            summary.get("mutating").and_then(serde_json::Value::as_u64),
        )
}

fn successful_tool_audit_rows_are_correlated(
    entries: &[serde_json::Value],
    provider_updates: &[serde_json::Value],
    expected_mutating: Option<u64>,
) -> bool {
    if entries.len() != provider_updates.len() {
        return false;
    }

    let mut updates_by_invocation = std::collections::BTreeMap::new();
    let mut update_provider_call_ids = std::collections::BTreeSet::new();
    for update in provider_updates {
        let Some(invocation_id) = update
            .get("tool_invocation_id")
            .and_then(serde_json::Value::as_str)
        else {
            return false;
        };
        let Some(provider_call_id) = update
            .get("provider_tool_call_id")
            .and_then(serde_json::Value::as_str)
        else {
            return false;
        };
        if !qualification_safe_id(invocation_id, 128)
            || !qualification_safe_id(provider_call_id, 256)
            || update
                .get("correlation_state")
                .and_then(serde_json::Value::as_str)
                != Some("OBSERVED")
            || updates_by_invocation
                .insert(invocation_id, provider_call_id)
                .is_some()
            || !update_provider_call_ids.insert(provider_call_id)
        {
            return false;
        }
    }

    let mut invocation_ids = std::collections::BTreeSet::new();
    let mut entry_provider_call_ids = std::collections::BTreeSet::new();
    let mut callback_ids = std::collections::BTreeSet::new();
    let mut sequences = std::collections::BTreeSet::new();
    let mut mutating_count = 0u64;
    for entry in entries {
        let Some(sequence) = entry.get("sequence").and_then(serde_json::Value::as_u64) else {
            return false;
        };
        if sequence == 0
            || sequence > entries.len() as u64
            || !sequences.insert(sequence)
            || entry
                .get("turn_completed")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
        {
            return false;
        }
        let Some(mutating) = entry.get("mutating").and_then(serde_json::Value::as_bool) else {
            return false;
        };
        if mutating {
            mutating_count = mutating_count.saturating_add(1);
        }
        let provider_name = entry
            .get("provider_tool_name")
            .and_then(serde_json::Value::as_str);
        let canonical_name = entry
            .get("canonical_tool_name")
            .and_then(serde_json::Value::as_str);
        let provider_tool_matches = provider_name
            .filter(|name| !name.is_empty() && name.trim() == *name)
            .and_then(orbit::tool_surface::CanonicalToolName::from_wire)
            .is_some_and(|tool| canonical_name == Some(tool.as_str()));
        let invocation_id = entry
            .get("tool_invocation_id")
            .and_then(serde_json::Value::as_str);
        let provider_call_id = entry
            .get("provider_tool_call_id")
            .and_then(serde_json::Value::as_str);
        let callback_id = entry
            .get("callback_request_id")
            .and_then(serde_json::Value::as_str);
        let provider_update_is_correlated = entry
            .get("provider_update_correlation")
            .and_then(serde_json::Value::as_str)
            == Some("CORRELATED")
            && entry
                .get("provider_update_title_class")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|class| matches!(class, "non_empty_string" | "empty_string"))
            && entry
                .get("provider_update_tool_kind")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|kind| {
                    matches!(
                        kind,
                        "read"
                            | "edit"
                            | "delete"
                            | "move"
                            | "search"
                            | "execute"
                            | "think"
                            | "fetch"
                            | "switch_mode"
                            | "other"
                    )
                })
            && entry
                .get("provider_update_status")
                .and_then(serde_json::Value::as_str)
                == Some("in_progress")
            && entry
                .get("provider_tool_call_id_shape")
                .and_then(serde_json::Value::as_str)
                == Some("string")
            && entry
                .get("callback_request_id_shape")
                .and_then(serde_json::Value::as_str)
                == Some("valid")
            && entry
                .get("terminal_state")
                .and_then(serde_json::Value::as_str)
                == Some("SUCCESS")
            && invocation_id.is_some_and(|id| qualification_safe_id(id, 128))
            && provider_call_id.is_some_and(|id| qualification_safe_id(id, 256))
            && callback_id.is_some_and(|id| qualification_safe_id(id, 130))
            && invocation_id.is_some_and(|id| invocation_ids.insert(id.to_owned()))
            && provider_call_id.is_some_and(|id| entry_provider_call_ids.insert(id.to_owned()))
            && callback_id.is_some_and(|id| callback_ids.insert(id.to_owned()))
            && invocation_id
                .and_then(|id| updates_by_invocation.remove(id))
                .is_some_and(|update_provider_call_id| {
                    Some(update_provider_call_id) == provider_call_id
                });
        if entry.get("outcome").and_then(serde_json::Value::as_str) != Some("SUCCESS")
            || entry
                .get("terminal_state")
                .and_then(serde_json::Value::as_str)
                != Some("SUCCESS")
            || !entry
                .get("error_code")
                .is_none_or(serde_json::Value::is_null)
            || !provider_update_is_correlated
            || entry
                .get("provider_name_mapping")
                .and_then(serde_json::Value::as_str)
                != Some("MATCH")
            || entry
                .get("advertised_to_provider")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
            || entry
                .get("role_allowed")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
            || !provider_tool_matches
        {
            return false;
        }
    }
    updates_by_invocation.is_empty()
        && sequences.len() == entries.len()
        && expected_mutating == Some(mutating_count)
}

fn qualification_safe_id(value: &str, limit: usize) -> bool {
    !value.is_empty()
        && value.len() <= limit
        && value.is_ascii()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

async fn run_live_cli_qualification(
    repository_path: &Path,
    database_url_file: Option<&Path>,
    output_format: Output,
) -> Result<()> {
    let repository = validate_live_qualification_repository(repository_path)?;
    let verification_environment = pinned_qualification_verification_environment().await?;
    let database_url = read_private_database_url(database_url_file).await?;
    let (engine, scratch) = connect_durable_catalog_engine(database_url.as_str()).await?;
    let workflow_store = orbit::workflow::WorkflowStore::new(engine.pool.clone());
    let verification_store = orbit::verification::VerificationStore::new(engine.pool.clone());
    let regression_store = orbit::regression_strategy::RegressionStore::new(engine.pool.clone());
    let policies = live_cli_qualification_policies();

    verification_store
        .save_policy(&policies.verification)
        .await?;
    regression_store
        .insert_selection_policy(&policies.selection)
        .await?;
    regression_store
        .insert_regression_policy(&policies.regression)
        .await?;

    let task_id = format!("task-live-cli-qualification-{}", orbit::model::id());
    let attempt_id = format!("attempt-live-cli-qualification-{}", orbit::model::id());
    let task = format!(
        "This is the fixed live CLI qualification task. In this disposable repository, replace README.md with exactly one line containing `{LIVE_QUALIFICATION_README_CONTENT}`. The committed `test.sh` is a fixed qualification harness: do not modify it or any other file. Return a structured implementation handoff."
    );
    let workflow = workflow_store
        .create_workflow_run_full(
            &task_id,
            &attempt_id,
            2,
            Some(&policies.verification),
            Some(&policies.regression),
            Some(&policies.selection),
            Some(&task),
            Some(
                repository
                    .path
                    .to_str()
                    .context("qualification repository path is not UTF-8")?,
            ),
            Some(&repository.base_revision),
        )
        .await
        .map_err(|_| anyhow::anyhow!("live CLI qualification workflow could not be created"))?;

    let coordinator = orbit::workflow_coordinator::WorkflowCoordinator::new(
        engine.pool.clone(),
        std::sync::Arc::new(orbit::workflow_coordinator::RealAcpRoleExecutor),
    )
    .with_verification_environment(verification_environment.clone())?;
    let observed_lock_owners = std::sync::Arc::new(tokio::sync::Mutex::new(
        std::collections::BTreeSet::<String>::new(),
    ));
    let lock_observer_pool = engine.pool.clone();
    let lock_observer_attempt_id = attempt_id.clone();
    let lock_observer_owners = observed_lock_owners.clone();
    let lock_observer = tokio::spawn(async move {
        loop {
            if let Ok(Some(owner)) = sqlx::query_scalar::<_, String>(
                "SELECT holder_role_execution_id FROM orbit_attempt_workspace_locks WHERE attempt_id = $1 AND revoked_at_ms IS NULL",
            )
            .bind(&lock_observer_attempt_id)
            .fetch_optional(&lock_observer_pool)
            .await
            {
                lock_observer_owners.lock().await.insert(owner);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    });
    let coordinator_error = coordinator.run_to_completion(&workflow.id).await.is_err();
    lock_observer.abort();
    let _ = lock_observer.await;
    let observed_lock_owners = observed_lock_owners.lock().await.clone();
    drop(coordinator);

    let final_workflow = workflow_store
        .get_workflow_run(&workflow.id)
        .await?
        .context("live CLI qualification workflow record disappeared")?;
    let role_executions = workflow_store.list_role_executions(&workflow.id).await?;
    let latest_review = workflow_store
        .get_latest_handoff_of_type(&workflow.id, orbit::workflow::HandoffType::Review)
        .await?;
    let review = latest_review.as_ref().and_then(|handoff| {
        serde_json::from_value::<orbit::workflow::ReviewDecision>(
            handoff.structured_payload.clone(),
        )
        .ok()
        .map(|decision| (handoff, decision))
    });
    let verification_runs = verification_store.list_runs(&attempt_id).await?;
    let final_candidate_id = final_workflow.current_workspace_state_id.as_deref();
    let run_for_tier = |tier| {
        verification_runs.iter().find(|run| {
            run.tier == Some(tier) && Some(run.workspace_state_id.as_str()) == final_candidate_id
        })
    };
    let fast_run = run_for_tier(orbit::regression_strategy::VerificationTier::Fast);
    let standard_run = run_for_tier(orbit::regression_strategy::VerificationTier::Standard);
    let full_run = run_for_tier(orbit::regression_strategy::VerificationTier::Full);
    let fast_selection =
        if let Some(selection_id) = fast_run.and_then(|run| run.selection_id.as_deref()) {
            regression_store.get_selection(selection_id).await?
        } else {
            None
        };
    let standard_selection =
        if let Some(selection_id) = standard_run.and_then(|run| run.selection_id.as_deref()) {
            regression_store.get_selection(selection_id).await?
        } else {
            None
        };
    let full_selection =
        if let Some(selection_id) = full_run.and_then(|run| run.selection_id.as_deref()) {
            regression_store.get_selection(selection_id).await?
        } else {
            None
        };
    let disk_workspace_state = orbit::workflow_coordinator::compute_workspace_state(
        &repository.path,
        &repository.base_revision,
    )
    .await
    .ok();
    let candidate_contract = candidate_contract_evidence(&repository);
    let candidate_contract_passed =
        candidate_contract["only_expected_readme_change"] == serde_json::Value::Bool(true);

    let mut all_agent_executions_terminal = true;
    let mut all_agent_cleanup_confirmed = true;
    let mut all_supervisor_exits_checked = true;
    let mut all_supervisor_exits_successful = true;
    let mut nonzero_supervisor_exit_count = 0u64;
    let mut missing_supervisor_exit_count = 0u64;
    let mut all_successful_agent_target_correlations = true;
    let mut all_role_agent_rows_linked = true;
    let mut all_successful_targets_have_credential_generation = true;
    let mut all_agent_tool_audits_reconciled = true;
    let mut implementer_mutation_tool_calls = 0u64;
    let mut implementer_tool_success_count = 0u64;
    let mut implementer_tool_failure_count = 0u64;
    let mut planner_reviewer_mutation_tool_calls = 0u64;
    let mut role_evidence = Vec::with_capacity(role_executions.len());
    let mut failed_agent_attempts = Vec::new();
    let mut observed_implementer_owner_ids = Vec::new();
    for role in &role_executions {
        let agents = sqlx::query(
            r#"
            SELECT id, role_execution_id, status, termination_reason, exit_code, provider,
                   requested_model, resolved_model, actual_model,
                   COALESCE(tool_call_count, 0) AS tool_call_count,
                   COALESCE(tool_success_count, 0) AS tool_success_count,
                   COALESCE(tool_failure_count, 0) AS tool_failure_count,
                   COALESCE(tool_counts, '{}'::jsonb) AS tool_counts,
                   metadata->>'cleanup_confirmed' AS cleanup_confirmed,
                   COALESCE(metadata->'tool_call_audit', 'null'::jsonb) AS tool_call_audit
            FROM orbit_agent_executions
            WHERE role_execution_id = $1
            ORDER BY started_at_ms, id
            "#,
        )
        .bind(&role.id)
        .fetch_all(&engine.pool)
        .await?;
        let mut agent_evidence = Vec::with_capacity(agents.len());
        for agent in agents {
            let id: String = agent.get("id");
            let linked_role_execution_id: Option<String> = agent.get("role_execution_id");
            let status: String = agent.get("status");
            let termination_reason: Option<String> = agent.get("termination_reason");
            let exit_code: Option<i32> = agent.get("exit_code");
            let provider: Option<String> = agent.get("provider");
            let requested_model: Option<String> = agent.get("requested_model");
            let resolved_model: Option<String> = agent.get("resolved_model");
            let actual_model: Option<String> = agent.get("actual_model");
            let tool_call_count: i64 = agent.get("tool_call_count");
            let tool_success_count: i64 = agent.get("tool_success_count");
            let tool_failure_count: i64 = agent.get("tool_failure_count");
            let raw_tool_counts: serde_json::Value = agent.get("tool_counts");
            let tool_counts = bounded_tool_counts(&raw_tool_counts);
            let cleanup_confirmed: Option<String> = agent.get("cleanup_confirmed");
            let cleanup_confirmed = cleanup_confirmed.as_deref() == Some("true");
            let tool_call_audit: serde_json::Value = agent.get("tool_call_audit");
            let tool_call_audit_reconciled = durable_tool_call_audit_is_strict(
                &tool_call_audit,
                tool_call_count,
                tool_success_count,
                tool_failure_count,
            );
            all_agent_tool_audits_reconciled &= tool_call_audit_reconciled;
            let tool_call_audit_diagnostic = orbit::workflow_coordinator::render_tool_call_audit(
                &serde_json::json!({"tool_call_audit": tool_call_audit}),
            );
            let role_agent_id_linked = role.agent_execution_ids.contains(&id);
            let selected_target_correlation = role.resolved_target.as_ref().is_some_and(|target| {
                role_agent_id_linked
                    && linked_role_execution_id.as_deref() == Some(role.id.as_str())
                    && provider.as_deref() == Some(target.provider.as_str())
                    && requested_model == target.requested_model
                    && resolved_model == target.resolved_model
            });
            all_agent_executions_terminal &=
                matches!(status.as_str(), "SUCCEEDED" | "FAILED" | "CANCELLED");
            all_agent_cleanup_confirmed &= cleanup_confirmed;
            all_role_agent_rows_linked &= role_agent_id_linked
                && linked_role_execution_id.as_deref() == Some(role.id.as_str());
            all_supervisor_exits_checked &= exit_code.is_some();
            all_supervisor_exits_successful &= exit_code == Some(0);
            if exit_code.is_some_and(|code| code != 0) {
                nonzero_supervisor_exit_count = nonzero_supervisor_exit_count.saturating_add(1);
            } else if exit_code.is_none() {
                missing_supervisor_exit_count = missing_supervisor_exit_count.saturating_add(1);
            }
            if status == "SUCCEEDED" {
                all_successful_agent_target_correlations &= selected_target_correlation;
                all_successful_targets_have_credential_generation &=
                    role.resolved_target.as_ref().is_some_and(|target| {
                        target
                            .credential_id
                            .as_deref()
                            .is_some_and(|reference| !reference.is_empty())
                            && target.credential_generation.is_some()
                    });
            }
            if role.role_id == "implementer" {
                implementer_mutation_tool_calls = implementer_mutation_tool_calls.saturating_add(
                    tool_count(&tool_counts, &["fs.write_text_file", "fs.edit_file"]),
                );
                implementer_tool_success_count = implementer_tool_success_count
                    .saturating_add(u64::try_from(tool_success_count.max(0)).unwrap_or(0));
                implementer_tool_failure_count = implementer_tool_failure_count
                    .saturating_add(u64::try_from(tool_failure_count.max(0)).unwrap_or(0));
                if role_agent_id_linked
                    && selected_target_correlation
                    && observed_lock_owners.contains(&role.id)
                    && tool_count(&tool_counts, &["fs.write_text_file", "fs.edit_file"]) > 0
                {
                    observed_implementer_owner_ids.push(role.id.clone());
                }
            } else if matches!(role.role_id.as_str(), "planner" | "reviewer") {
                planner_reviewer_mutation_tool_calls = planner_reviewer_mutation_tool_calls
                    .saturating_add(tool_count(
                        &tool_counts,
                        &["fs.write_text_file", "fs.edit_file"],
                    ));
            }
            if status != "SUCCEEDED" {
                failed_agent_attempts.push(serde_json::json!({
                    "role_execution_id": role.id,
                    "agent_execution_id": id,
                    "provider": provider,
                    "requested_model": requested_model,
                    "resolved_model": resolved_model,
                    "actual_model_observed": actual_model,
                    "tool_call_count": tool_call_count,
                    "tool_success_count": tool_success_count,
                    "tool_failure_count": tool_failure_count,
                    "attempt_status": status,
                    "supervisor_exit_code": exit_code,
                    "cleanup_confirmed": cleanup_confirmed,
                    "classification": "failed_agent_attempt; no fallback inferred from failure alone"
                }));
            }
            agent_evidence.push(serde_json::json!({
                "id": id,
                "status": status,
                "termination": if termination_reason.as_deref() == Some("completed") { "completed" } else { "not_completed_or_unknown" },
                "supervisor_exit_code": exit_code,
                "provider": provider,
                "requested_model": requested_model,
                "resolved_model": resolved_model,
                "actual_model_observed": actual_model,
                "cleanup_confirmed": cleanup_confirmed,
                "tool_call_count": tool_call_count,
                "tool_success_count": tool_success_count,
                "tool_failure_count": tool_failure_count,
                "tool_counts": tool_counts,
                "tool_call_audit": {
                    "strictly_reconciled": tool_call_audit_reconciled,
                    "sanitized_diagnostic": tool_call_audit_diagnostic
                },
                "role_execution_id_linked": linked_role_execution_id.as_deref() == Some(role.id.as_str()) && role_agent_id_linked,
                "selected_target_correlation": selected_target_correlation
            }));
        }
        all_role_agent_rows_linked &= !role.agent_execution_ids.is_empty()
            && role.agent_execution_ids.len() == agent_evidence.len();
        let (target, credential_quota_observations) = if let Some(target) =
            role.resolved_target.as_ref()
        {
            (
                Some(serde_json::json!({
                    "provider": target.provider,
                    "account_reference": target.credential_id,
                    "credential_generation": target.credential_generation,
                    "runtime_interface": target.runtime_interface,
                    "requested_model": target.requested_model,
                    "resolved_model": target.resolved_model,
                    "reasoning_effort": {
                        "requested": "not_configured",
                        "actual": "unknown_not_observed"
                    },
                    "quota_and_availability_selection": reset_aware_selection_evidence(&target.resolution_reason)
                })),
                selected_credential_quota_evidence(&engine.pool, target).await?,
            )
        } else {
            (
                None,
                serde_json::json!({"state":"unknown", "quota_windows":[]}),
            )
        };
        role_evidence.push(serde_json::json!({
            "role_execution_id": role.id,
            "role": role.role_id,
            "stage": role.stage,
            "status": role.status.as_str(),
            "input_workspace_state_id": role.input_workspace_state_id,
            "output_workspace_state_id": role.output_workspace_state_id,
            "resolved_target": target,
            "credential_quota_observations": credential_quota_observations,
            "agent_executions": agent_evidence
        }));
    }

    let review_workspace_state_id = latest_review
        .as_ref()
        .and_then(|handoff| handoff.workspace_state_id.as_deref());
    let reviewer_role_execution_id = latest_review
        .as_ref()
        .and_then(|handoff| handoff.role_execution_id.as_deref());
    let reviewer_role = reviewer_role_execution_id.and_then(|id| {
        role_executions
            .iter()
            .find(|role| role.id == id && role.role_id == "reviewer")
    });
    let reviewer_bound_to_reviewed_state = reviewer_role.is_some_and(|role| {
        role.status == orbit::workflow::RoleExecutionStatus::Succeeded
            && role.input_workspace_state_id.as_deref() == review_workspace_state_id
            && !role.agent_execution_ids.is_empty()
    });
    let review_approved = review.as_ref().is_some_and(|(_, decision)| {
        decision.decision == orbit::workflow::ReviewDecisionStatus::Approve
    }) && reviewer_bound_to_reviewed_state;
    let review_decision = review
        .as_ref()
        .map(|(_, decision)| match decision.decision {
            orbit::workflow::ReviewDecisionStatus::Approve => "APPROVE",
            orbit::workflow::ReviewDecisionStatus::ChangesRequested => "CHANGES_REQUESTED",
            orbit::workflow::ReviewDecisionStatus::Blocked => "BLOCKED",
        });
    let full_run_workspace_state_id = full_run.map(|run| run.workspace_state_id.as_str());
    let final_workspace_state_id = final_workflow.current_workspace_state_id.as_deref();
    let disk_workspace_state_id = disk_workspace_state
        .as_ref()
        .map(|state| state.state_id.as_str());
    let fast_passed = executed_fixed_check_passed(fast_run, &verification_environment)
        && fast_run.is_some_and(|run| {
            selection_contains_required_check(
                fast_selection.as_ref(),
                &run.workspace_state_id,
                orbit::regression_strategy::VerificationTier::Fast,
            )
        });
    let standard_passed = executed_fixed_check_passed(standard_run, &verification_environment)
        && standard_run.is_some_and(|run| {
            selection_contains_required_check(
                standard_selection.as_ref(),
                &run.workspace_state_id,
                orbit::regression_strategy::VerificationTier::Standard,
            )
        });
    let full_passed = executed_fixed_check_passed(full_run, &verification_environment)
        && full_run.is_some_and(|run| {
            run.tier == Some(orbit::regression_strategy::VerificationTier::Full)
                && selection_contains_required_check(
                    full_selection.as_ref(),
                    &run.workspace_state_id,
                    orbit::regression_strategy::VerificationTier::Full,
                )
        });

    let active_lock_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM orbit_attempt_workspace_locks WHERE attempt_id = $1 AND revoked_at_ms IS NULL",
    )
    .bind(&attempt_id)
    .fetch_one(&engine.pool)
    .await?;
    let step_owner_id: Option<String> =
        sqlx::query_scalar("SELECT step_owner_id FROM orbit_workflow_runs WHERE id = $1")
            .bind(&workflow.id)
            .fetch_one(&engine.pool)
            .await?;

    let states_match = review_workspace_state_id.is_some()
        && reviewer_role.is_some_and(|role| {
            role.input_workspace_state_id.as_deref() == review_workspace_state_id
        })
        && review_workspace_state_id == full_run_workspace_state_id
        && full_run_workspace_state_id == final_workspace_state_id
        && final_workspace_state_id == disk_workspace_state_id;
    let expected_roles = ["planner", "implementer", "reviewer"];
    let roles_succeeded = expected_roles.iter().all(|expected| {
        role_executions.iter().any(|role| {
            role.role_id == *expected
                && role.status == orbit::workflow::RoleExecutionStatus::Succeeded
        })
    });
    let all_role_executions_succeeded = role_executions
        .iter()
        .all(|role| role.status == orbit::workflow::RoleExecutionStatus::Succeeded);
    let repair_iteration_count = role_executions
        .iter()
        .filter(|role| role.role_id == "implementer" && role.stage == "REPAIRING")
        .count();
    let implementer_role_ids = role_executions
        .iter()
        .filter(|role| role.role_id == "implementer")
        .map(|role| role.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let observed_implementer_owner_role_execution_ids = observed_implementer_owner_ids
        .into_iter()
        .filter(|id| {
            observed_lock_owners.contains(id) && implementer_role_ids.contains(id.as_str())
        })
        .collect::<std::collections::BTreeSet<_>>();
    let implementer_mutation_owner_observed = !observed_implementer_owner_role_execution_ids
        .is_empty()
        && implementer_mutation_tool_calls > 0
        && implementer_tool_success_count > 0;
    let read_only_roles_had_no_mutation_calls = planner_reviewer_mutation_tool_calls == 0;
    let reset_aware_targets_recorded = expected_roles.iter().all(|expected| {
        role_executions
            .iter()
            .filter(|role| role.role_id == *expected)
            .any(|role| {
                role.resolved_target
                    .as_ref()
                    .is_some_and(|target| target.resolution_reason.starts_with("reset-aware rank="))
            })
    });
    let cleanup_confirmed = all_agent_executions_terminal
        && all_agent_cleanup_confirmed
        && all_supervisor_exits_checked
        && all_supervisor_exits_successful
        && role_executions
            .iter()
            .all(|role| !role.agent_execution_ids.is_empty());
    let accepted = !coordinator_error
        && final_workflow.status == orbit::workflow::WorkflowStage::Completed
        && review_approved
        && candidate_contract_passed
        && reset_aware_targets_recorded
        && fast_passed
        && standard_passed
        && full_passed
        && states_match
        && roles_succeeded
        && all_role_executions_succeeded
        && reviewer_bound_to_reviewed_state
        && all_successful_agent_target_correlations
        && all_successful_targets_have_credential_generation
        && all_role_agent_rows_linked
        && all_agent_tool_audits_reconciled
        && implementer_mutation_owner_observed
        && read_only_roles_had_no_mutation_calls
        && cleanup_confirmed
        && active_lock_count == 0
        && step_owner_id.is_none();

    let verification_evidence: Vec<_> = verification_runs
        .iter()
        .map(|run| {
            serde_json::json!({
                "id": run.id,
                "tier": run.tier,
                "result": run.overall_result,
                "workspace_state_id": run.workspace_state_id,
                "selection_id": run.selection_id,
                "environment": {
                    "execution_profile": run.environment_identity.execution_profile,
                    "isolation": run.environment_identity.isolation,
                    "runtime_image": run.environment_identity.runtime_image,
                    "runtime_image_digest": run.environment_identity.runtime_image_digest,
                    "oci_runtime": run.environment_identity.oci_runtime,
                    "network_policy": run.environment_identity.network_policy,
                    "cache_policy": run.environment_identity.cache_policy
                },
                "steps": run.step_runs.iter().map(|step| serde_json::json!({
                    "id": step.id,
                    "step_id": step.step_id,
                    "status": step.status,
                    "exit_code": step.exit_code,
                    "stdout_marker_observed": step.stdout_preview.as_deref().is_some_and(|stdout| stdout.contains(LIVE_QUALIFICATION_TEST_MARKER))
                })).collect::<Vec<_>>()
            })
        })
        .collect();
    let report = serde_json::json!({
        "qualification": "ORBIT_LIVE_WORKFLOW_QUALIFICATION",
        "task_id": task_id,
        "attempt_id": attempt_id,
        "workflow_id": workflow.id,
        "workflow_status": final_workflow.status,
        "coordinator_error": coordinator_error,
        "role_executions": role_evidence,
        "reset_aware_targets_recorded": reset_aware_targets_recorded,
        "fallback_events": [],
        "fallback_behavior": "The current coordinator resolves one target per role and has no execution-time fallback loop; failed agent attempts are listed separately.",
        "failed_agent_attempts": failed_agent_attempts,
        "repair_iteration_count": repair_iteration_count,
        "agent_target_correlations": {
            "all_successful_agent_targets_match_persisted_role_targets": all_successful_agent_target_correlations,
            "all_successful_targets_have_selected_credential_generation": all_successful_targets_have_credential_generation,
            "all_role_agent_rows_linked_to_durable_role_execution_ids": all_role_agent_rows_linked,
            "credential_identity_and_generation_source": "persisted RoleExecution.resolved_target linked to each AgentExecution by role_execution_id and agent_execution_ids"
        },
        "all_agent_tool_audits_reconciled": all_agent_tool_audits_reconciled,
        "tool_call_audit_acceptance": "durable totals and outcomes must match persisted agent counters; every row must be successful; unmatched updates and omitted audit evidence must be zero",
        "mutation_authority": {
            "observed_active_lock_owner_role_execution_ids": observed_lock_owners,
            "implementer_mutation_owner_role_execution_ids": observed_implementer_owner_role_execution_ids,
            "implementer_write_or_edit_tool_calls": implementer_mutation_tool_calls,
            "implementer_tool_success_count": implementer_tool_success_count,
            "implementer_tool_failure_count": implementer_tool_failure_count,
            "implementer_agent_record_correlated_to_observed_lock_owner": implementer_mutation_owner_observed,
            "planner_reviewer_mutation_tool_calls": planner_reviewer_mutation_tool_calls,
            "planner_reviewer_read_only_tool_calls_observed": read_only_roles_had_no_mutation_calls
        },
        "review": {
            "artifact_id": latest_review.as_ref().map(|handoff| handoff.id.as_str()),
            "reviewer_role_execution_id": reviewer_role_execution_id,
            "reviewer_input_workspace_state_id": reviewer_role.and_then(|role| role.input_workspace_state_id.as_deref()),
            "reviewer_bound_to_reviewed_workspace_state": reviewer_bound_to_reviewed_state,
            "decision": review_decision,
            "workspace_state_id": review_workspace_state_id
        },
        "candidate_contract": candidate_contract,
        "verification_runs": verification_evidence,
        "tier_acceptance": {
            "fast": {"run_id": fast_run.map(|run| run.id.as_str()), "passed": fast_passed, "selection_id": fast_selection.as_ref().map(|selection| selection.id.as_str())},
            "standard": {"run_id": standard_run.map(|run| run.id.as_str()), "passed": standard_passed, "selection_id": standard_selection.as_ref().map(|selection| selection.id.as_str())},
            "full": {"run_id": full_run.map(|run| run.id.as_str()), "passed": full_passed, "selection_id": full_selection.as_ref().map(|selection| selection.id.as_str())}
        },
        "full_selection": full_selection.as_ref().map(|selection| serde_json::json!({
            "id": selection.id,
            "requested_tier": selection.requested_tier,
            "workspace_state_id": selection.workspace_state_id,
            "selected_checks": selection.selected_checks.iter().map(|check| &check.check_id).collect::<Vec<_>>()
        })),
        "workspace_state_ids": {
            "reviewed": review_workspace_state_id,
            "full": full_run_workspace_state_id,
            "workflow_final": final_workspace_state_id,
            "on_disk": disk_workspace_state_id
        },
        "cleanup": {
            "all_agent_executions_terminal": all_agent_executions_terminal,
            "all_supervisor_exits_checked": all_supervisor_exits_checked,
            "all_supervisor_exits_successful": all_supervisor_exits_successful,
            "nonzero_supervisor_exit_count": nonzero_supervisor_exit_count,
            "missing_supervisor_exit_count": missing_supervisor_exit_count,
            "all_role_cleanup_receipts_confirmed": all_agent_cleanup_confirmed,
            "no_active_role_processes_confirmed_by_terminal_supervisor_receipts": cleanup_confirmed,
            "verification_container_cleanup_confirmed_by_passing_isolated_commands": fast_passed && standard_passed && full_passed,
            "active_workspace_mutation_locks": active_lock_count,
            "active_workflow_step_owner": step_owner_id
        },
        "accepted": accepted
    });

    engine.pool.close().await;
    drop(scratch);
    match output_format {
        Output::Json | Output::Text => println!("{}", serde_json::to_string_pretty(&report)?),
        Output::Jsonl => println!("{}", serde_json::to_string(&report)?),
    }
    anyhow::ensure!(
        accepted,
        "live CLI qualification did not satisfy acceptance; inspect sanitized evidence"
    );
    Ok(())
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
    if let Commands::Workflow(WorkflowArgs { action }) = &cli.command {
        ensure_cli_workflow_execution_enabled(action)?;
        match action {
            WorkflowAction::QualifyLive {
                repo,
                database_url_file,
            } => {
                run_live_cli_qualification(repo, database_url_file.as_deref(), output_format)
                    .await?;
                return Ok(());
            }
            WorkflowAction::Start {
                pos_task_id,
                pos_attempt_id,
                task_id,
                attempt_id,
                task,
                task_file,
                repo,
                base_revision,
                kind: _,
                max_iterations,
                policy,
                regression_policy,
                selection_policy,
                detach,
                database_url_file,
            } => {
                let database_url = read_private_database_url(database_url_file.as_deref()).await?;
                let (engine, scratch) =
                    connect_durable_catalog_engine(database_url.as_str()).await?;
                let store = orbit::workflow::WorkflowStore::new(engine.pool.clone());
                let ver_store = orbit::verification::VerificationStore::new(engine.pool.clone());
                let reg_store =
                    orbit::regression_strategy::RegressionStore::new(engine.pool.clone());

                let task_id = task_id
                    .clone()
                    .or_else(|| pos_task_id.clone())
                    .unwrap_or_else(|| format!("task-{}", orbit::model::id()));
                let attempt_id = attempt_id
                    .clone()
                    .or_else(|| pos_attempt_id.clone())
                    .unwrap_or_else(|| format!("att-{}", orbit::model::id()));

                let task_prompt = if let Some(t) = task {
                    Some(t.clone())
                } else if let Some(tf) = task_file {
                    Some(
                        tokio::fs::read_to_string(tf)
                            .await
                            .context("reading task file")?,
                    )
                } else {
                    None
                };

                let policy_def = if let Some(pol_path) = policy {
                    let data = tokio::fs::read_to_string(pol_path)
                        .await
                        .context("reading verification policy file")?;
                    let p: orbit::verification::VerificationPolicy =
                        serde_json::from_str(&data).context("parsing verification policy")?;
                    p.validate()?;
                    ver_store.save_policy(&p).await?;
                    Some(p)
                } else {
                    None
                };

                let reg_policy_def = if let Some(reg_path) = regression_policy {
                    let data = tokio::fs::read_to_string(reg_path)
                        .await
                        .context("reading regression policy file")?;
                    let p: orbit::regression_strategy::RegressionPolicy =
                        serde_json::from_str(&data).context("parsing regression policy")?;
                    reg_store.insert_regression_policy(&p).await?;
                    Some(p)
                } else {
                    None
                };

                let sel_policy_def = if let Some(sel_path) = selection_policy {
                    let data = tokio::fs::read_to_string(sel_path)
                        .await
                        .context("reading selection policy file")?;
                    let p: orbit::regression_strategy::SelectionPolicy =
                        serde_json::from_str(&data).context("parsing selection policy")?;
                    reg_store.insert_selection_policy(&p).await?;
                    Some(p)
                } else {
                    None
                };

                let repo_path = repo
                    .as_ref()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|| ".".to_string());
                let base_rev = base_revision.clone().unwrap_or_else(|| "HEAD".to_string());

                let wf = store
                    .create_workflow_run_full(
                        &task_id,
                        &attempt_id,
                        *max_iterations,
                        policy_def.as_ref(),
                        reg_policy_def.as_ref(),
                        sel_policy_def.as_ref(),
                        task_prompt.as_deref(),
                        Some(&repo_path),
                        Some(&base_rev),
                    )
                    .await?;

                if *detach {
                    match output_format {
                        Output::Text => {
                            println!("{}", orbit::workflow::format_workflow_show(&wf, &[]));
                        }
                        Output::Json => println!("{}", serde_json::to_string_pretty(&wf)?),
                        Output::Jsonl => println!("{}", serde_json::to_string(&wf)?),
                    }
                    engine.pool.close().await;
                    drop(scratch);
                    return Ok(());
                }

                let executor =
                    std::sync::Arc::new(orbit::workflow_coordinator::RealAcpRoleExecutor);
                let coordinator = orbit::workflow_coordinator::WorkflowCoordinator::new(
                    engine.pool.clone(),
                    executor,
                );
                let final_stage = coordinator.run_to_completion(&wf.id).await?;
                let wf_final = store
                    .get_workflow_run(&wf.id)
                    .await?
                    .context("workflow run not found")?;
                let roles = store.list_role_executions(&wf.id).await?;

                match output_format {
                    Output::Text => {
                        println!(
                            "{}",
                            orbit::workflow::format_workflow_show(&wf_final, &roles)
                        );
                    }
                    Output::Json => println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "workflow": wf_final,
                            "role_executions": roles,
                        }))?
                    ),
                    Output::Jsonl => println!(
                        "{}",
                        serde_json::to_string(&serde_json::json!({
                            "workflow": wf_final,
                            "role_executions": roles,
                        }))?
                    ),
                }
                engine.pool.close().await;
                drop(scratch);
                if matches!(
                    final_stage.status,
                    orbit::workflow::WorkflowStage::Failed
                        | orbit::workflow::WorkflowStage::Exhausted
                ) {
                    std::process::exit(1);
                }
                return Ok(());
            }
            WorkflowAction::Run {
                workflow_run_id,
                database_url_file,
            } => {
                let database_url = read_private_database_url(database_url_file.as_deref()).await?;
                let (engine, scratch) =
                    connect_durable_catalog_engine(database_url.as_str()).await?;
                let store = orbit::workflow::WorkflowStore::new(engine.pool.clone());
                let executor =
                    std::sync::Arc::new(orbit::workflow_coordinator::RealAcpRoleExecutor);
                let coordinator = orbit::workflow_coordinator::WorkflowCoordinator::new(
                    engine.pool.clone(),
                    executor,
                );
                let final_stage = coordinator.run_to_completion(workflow_run_id).await?;
                let wf_final = store
                    .get_workflow_run(workflow_run_id)
                    .await?
                    .context("workflow run not found")?;
                let roles = store.list_role_executions(workflow_run_id).await?;

                match output_format {
                    Output::Text => {
                        println!(
                            "{}",
                            orbit::workflow::format_workflow_show(&wf_final, &roles)
                        );
                    }
                    Output::Json => println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "workflow": wf_final,
                            "role_executions": roles,
                        }))?
                    ),
                    Output::Jsonl => println!(
                        "{}",
                        serde_json::to_string(&serde_json::json!({
                            "workflow": wf_final,
                            "role_executions": roles,
                        }))?
                    ),
                }
                engine.pool.close().await;
                drop(scratch);
                if matches!(
                    final_stage.status,
                    orbit::workflow::WorkflowStage::Failed
                        | orbit::workflow::WorkflowStage::Exhausted
                ) {
                    std::process::exit(1);
                }
                return Ok(());
            }
            WorkflowAction::Show {
                workflow_run_id,
                database_url_file,
            } => {
                let database_url = read_private_database_url(database_url_file.as_deref()).await?;
                let (engine, scratch) =
                    connect_durable_catalog_engine(database_url.as_str()).await?;
                let store = orbit::workflow::WorkflowStore::new(engine.pool.clone());
                let wf = store
                    .get_workflow_run(workflow_run_id)
                    .await?
                    .context("workflow run not found")?;
                let roles = store.list_role_executions(workflow_run_id).await?;

                match output_format {
                    Output::Text => {
                        println!("{}", orbit::workflow::format_workflow_show(&wf, &roles));
                    }
                    Output::Json => println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "workflow": wf,
                            "role_executions": roles,
                        }))?
                    ),
                    Output::Jsonl => println!(
                        "{}",
                        serde_json::to_string(&serde_json::json!({
                            "workflow": wf,
                            "role_executions": roles,
                        }))?
                    ),
                }
                engine.pool.close().await;
                drop(scratch);
                return Ok(());
            }
            WorkflowAction::List {
                attempt,
                database_url_file,
            } => {
                let database_url = read_private_database_url(database_url_file.as_deref()).await?;
                let (engine, scratch) =
                    connect_durable_catalog_engine(database_url.as_str()).await?;
                let store = orbit::workflow::WorkflowStore::new(engine.pool.clone());
                let list = store.list_workflow_runs(attempt.as_deref()).await?;

                match output_format {
                    Output::Json | Output::Text => {
                        println!("{}", serde_json::to_string_pretty(&list)?);
                    }
                    Output::Jsonl => println!("{}", serde_json::to_string(&list)?),
                }
                engine.pool.close().await;
                drop(scratch);
                return Ok(());
            }
            WorkflowAction::Cancel {
                workflow_run_id,
                reason,
                database_url_file,
            } => {
                let database_url = read_private_database_url(database_url_file.as_deref()).await?;
                let (engine, scratch) =
                    connect_durable_catalog_engine(database_url.as_str()).await?;
                let executor =
                    std::sync::Arc::new(orbit::workflow_coordinator::RealAcpRoleExecutor);
                let coordinator = orbit::workflow_coordinator::WorkflowCoordinator::new(
                    engine.pool.clone(),
                    executor,
                );
                let cancelled = coordinator.cancel_workflow(workflow_run_id, reason).await?;

                match output_format {
                    Output::Text => {
                        println!("Workflow {} CANCELLED: {}", cancelled.id, reason);
                    }
                    Output::Json => println!("{}", serde_json::to_string_pretty(&cancelled)?),
                    Output::Jsonl => println!("{}", serde_json::to_string(&cancelled)?),
                }
                engine.pool.close().await;
                drop(scratch);
                return Ok(());
            }
        }
    }
    if let Commands::Verification(VerificationArgs { action }) = &cli.command {
        match action {
            VerificationAction::Run {
                attempt_id,
                workspace,
                plan,
                image,
                policy,
                database_url_file,
            } => {
                anyhow::ensure!(
                    image.is_some(),
                    "VERIFICATION_PROFILE_REQUIRED: host verification is disabled; select a pinned isolated image"
                );
                let database_url = read_private_database_url(database_url_file.as_deref()).await?;
                let (engine, scratch) =
                    connect_durable_catalog_engine(database_url.as_str()).await?;
                let plan_bytes = tokio::fs::read(plan).await?;
                let plan_def: orbit::verification::VerificationPlan =
                    serde_json::from_slice(&plan_bytes)
                        .or_else(|_| serde_yaml::from_slice(&plan_bytes))?;
                plan_def.validate()?;

                let head = String::from_utf8(
                    tokio::process::Command::new("git")
                        .args(["-C", &workspace.to_string_lossy(), "rev-parse", "HEAD"])
                        .output()
                        .await?
                        .stdout,
                )
                .unwrap_or_else(|_| "unknown".into())
                .trim()
                .to_string();

                let diff_bytes = tokio::process::Command::new("git")
                    .args(["-C", &workspace.to_string_lossy(), "diff", "HEAD"])
                    .output()
                    .await?
                    .stdout;
                let diff_sha256 = if diff_bytes.is_empty() {
                    None
                } else {
                    Some(orbit::model::digest(&diff_bytes))
                };

                let ws_state = orbit::verification::WorkspaceState::compute_from_parts(
                    &head,
                    &head,
                    diff_sha256.as_deref(),
                );
                let store = orbit::verification::VerificationStore::new(engine.pool.clone());
                let (profile_name, isolation, runtime_img, oci_rt) = if let Some(img) = image {
                    let mut inspect = tokio::process::Command::new("podman");
                    inspect.args(["image", "inspect", img, "--format", "{{.Id}}"]);
                    let digest = inspect.output().await.ok().and_then(|o| {
                        if o.status.success() {
                            Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                        } else {
                            None
                        }
                    });
                    (
                        "sandboxed-container".to_string(),
                        "rootless-podman".to_string(),
                        Some(img.clone()),
                        digest,
                    )
                } else {
                    (
                        "local-operator".to_string(),
                        "process-group".to_string(),
                        None,
                        None,
                    )
                };

                let policy_def: Option<orbit::verification::VerificationPolicy> =
                    if let Some(pol_path) = policy {
                        let pol_raw =
                            tokio::fs::read_to_string(&pol_path)
                                .await
                                .with_context(|| {
                                    format!("cannot read policy file: {}", pol_path.display())
                                })?;
                        let p: orbit::verification::VerificationPolicy =
                            serde_json::from_str(&pol_raw).with_context(|| {
                                format!("invalid policy JSON in {}", pol_path.display())
                            })?;
                        p.validate()?;
                        store.save_policy(&p).await?;
                        Some(p)
                    } else {
                        None
                    };

                let (net_pol, cache_pol, env_pol_digest, int_env_digest) =
                    if let Some(p) = &policy_def {
                        (
                            p.network_policy,
                            p.cache_policy,
                            Some(p.environment_policy.digest()),
                            p.integration_environment_spec.as_ref().map(|s| s.digest()),
                        )
                    } else {
                        (
                            orbit::verification::VerificationNetworkPolicy::None,
                            orbit::verification::VerificationCachePolicy::Clean,
                            None,
                            None,
                        )
                    };

                let env = orbit::verification::EnvironmentIdentity {
                    execution_profile: profile_name,
                    isolation,
                    runtime_image: runtime_img,
                    runtime_image_digest: oci_rt,
                    oci_runtime: if image.is_some() {
                        Some("podman".into())
                    } else {
                        None
                    },
                    network_policy: net_pol,
                    cache_policy: cache_pol,
                    environment_policy_digest: env_pol_digest,
                    integration_environment_digest: int_env_digest,
                    browser_verification_digest: None,
                    browser_runtime_image_digest: None,
                    regression_policy_digest: None,
                    selection_digest: None,
                    architecture: std::env::consts::ARCH.into(),
                    os: std::env::consts::OS.into(),
                    orbit_version: env!("CARGO_PKG_VERSION").into(),
                };

                let run = orbit::verification::execute_verification_plan_with_policy(
                    &store,
                    attempt_id,
                    &ws_state,
                    &plan_def,
                    workspace,
                    env,
                    policy_def.as_ref(),
                    None,
                )
                .await?;

                match output_format {
                    Output::Text => {
                        println!("{}", orbit::verification::format_verification_show(&run))
                    }
                    Output::Json => println!("{}", serde_json::to_string_pretty(&run)?),
                    Output::Jsonl => println!("{}", serde_json::to_string(&run)?),
                }
                engine.pool.close().await;
                drop(scratch);
                return Ok(());
            }
            VerificationAction::Show {
                run_id,
                database_url_file,
            } => {
                let database_url = read_private_database_url(database_url_file.as_deref()).await?;
                let (engine, scratch) =
                    connect_durable_catalog_engine(database_url.as_str()).await?;
                let store = orbit::verification::VerificationStore::new(engine.pool.clone());
                let run = store
                    .get_run(run_id)
                    .await?
                    .context("verification run not found")?;

                match output_format {
                    Output::Text => {
                        println!("{}", orbit::verification::format_verification_show(&run))
                    }
                    Output::Json => println!("{}", serde_json::to_string_pretty(&run)?),
                    Output::Jsonl => println!("{}", serde_json::to_string(&run)?),
                }
                engine.pool.close().await;
                drop(scratch);
                return Ok(());
            }
            VerificationAction::List {
                attempt_id,
                database_url_file,
            } => {
                let database_url = read_private_database_url(database_url_file.as_deref()).await?;
                let (engine, scratch) =
                    connect_durable_catalog_engine(database_url.as_str()).await?;
                let store = orbit::verification::VerificationStore::new(engine.pool.clone());
                let runs = store.list_runs(attempt_id).await?;

                match output_format {
                    Output::Json | Output::Text => {
                        println!("{}", serde_json::to_string_pretty(&runs)?)
                    }
                    Output::Jsonl => println!("{}", serde_json::to_string(&runs)?),
                }
                engine.pool.close().await;
                drop(scratch);
                return Ok(());
            }
        }
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
        let removed = CredentialStore::new(&engine.pool)
            .hard_delete(reference, &backend)
            .await?;
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
        Commands::Verification(_) => {
            unreachable!("local verification handled before API credential resolution")
        }
        Commands::Workflow(_) => {
            unreachable!("local workflow handled before API credential resolution")
        }
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
mod workflow_execution_gate_tests {
    use super::*;

    #[test]
    fn production_start_and_resume_are_gated_before_database_access() {
        let start = WorkflowAction::Start {
            pos_task_id: None,
            pos_attempt_id: None,
            task_id: None,
            attempt_id: None,
            task: Some("task".into()),
            task_file: None,
            repo: None,
            base_revision: None,
            kind: "software-change".into(),
            max_iterations: 3,
            policy: None,
            regression_policy: None,
            selection_policy: None,
            detach: true,
            database_url_file: None,
        };
        let resume = WorkflowAction::Run {
            workflow_run_id: "wf-1".into(),
            database_url_file: None,
        };
        let show = WorkflowAction::Show {
            workflow_run_id: "wf-1".into(),
            database_url_file: None,
        };
        let qualify_live = WorkflowAction::QualifyLive {
            repo: PathBuf::from("/tmp/orbit-live-cli-candidate"),
            database_url_file: None,
        };

        for action in [&start, &resume] {
            let error = ensure_cli_workflow_execution_enabled(action).unwrap_err();
            assert!(error.to_string().contains("CLI_WORKFLOW_EXECUTION_GATED"));
        }
        assert!(ensure_cli_workflow_execution_enabled(&show).is_ok());
        assert!(ensure_cli_workflow_execution_enabled(&qualify_live).is_ok());
    }
}

#[cfg(test)]
mod live_workflow_qualification_tests {
    use super::*;

    fn git(repository: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output()
            .expect("git should be installed for repository safety tests");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn clean_temp_git_repository(parent: &Path) -> PathBuf {
        let repository = parent.join("candidate");
        fs::create_dir(&repository).expect("create test repository");
        let init = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&repository)
            .output()
            .expect("git init should run");
        assert!(init.status.success());
        git(
            &repository,
            &["config", "user.name", "Live Qualification Test"],
        );
        git(
            &repository,
            &["config", "user.email", "live-qualification@example.invalid"],
        );
        fs::write(
            repository.join("README.md"),
            "Disposable live CLI candidate\n",
        )
        .expect("write qualification README");
        fs::write(repository.join("test.sh"), LIVE_QUALIFICATION_TEST_SCRIPT)
            .expect("write fixed live qualification contract script");
        git(&repository, &["add", "README.md", "test.sh"]);
        git(
            &repository,
            &["commit", "--quiet", "-m", "fixture baseline"],
        );
        repository
    }

    #[test]
    fn qualify_live_parser_requires_repository_and_has_no_task_or_policy_overrides() {
        let parsed = Cli::try_parse_from([
            "orbit",
            "workflow",
            "qualify-live",
            "--repo",
            "/tmp/orbit-live-cli-candidate",
        ]);
        assert!(parsed.is_ok());

        let missing_repository = Cli::try_parse_from(["orbit", "workflow", "qualify-live"]);
        assert!(missing_repository.is_err());

        let caller_override = Cli::try_parse_from([
            "orbit",
            "workflow",
            "qualify-live",
            "--repo",
            "/tmp/orbit-live-cli-candidate",
            "--task",
            "weaker check",
        ]);
        assert!(caller_override.is_err());
    }

    #[test]
    fn live_qualification_rejects_unmatched_or_inconsistent_durable_tool_audits() {
        let audit = |total, successful, unsuccessful, unmatched| {
            serde_json::json!({
                "schema_version": 2,
                "summary": {
                    "total": total,
                    "callback_count": total - unmatched,
                    "provider_notification_count": total,
                    "successful": successful,
                    "unsuccessful": unsuccessful,
                    "mutating": 0,
                    "unmatched_provider_calls": unmatched,
                    "unmatched_callbacks": 0,
                    "denied": 0,
                    "mutating_unknown": 0
                },
                "correlation_capability": "SUPPORTED",
                "entries": (0..total).map(|index| serde_json::json!({
                    "sequence": index + 1,
                    "tool_invocation_id": format!("oti-{index}"),
                    "provider_tool_call_id": format!("call-{index}"),
                    "callback_request_id": format!("s:orbit-{index}"),
                    "callback_request_id_shape": "valid",
                    "provider_tool_name": "fs/read_text_file",
                    "provider_name_mapping": "MATCH",
                    "provider_update_correlation": "CORRELATED",
                    "provider_update_title_class": "non_empty_string",
                    "provider_update_tool_kind": "other",
                    "provider_update_status": "in_progress",
                    "provider_tool_call_id_shape": "string",
                    "canonical_tool_name": "fs.read_text_file",
                    "advertised_to_provider": true,
                    "role_allowed": true,
                    "outcome": if index < successful { "SUCCESS" } else { "EXECUTION_FAILURE" },
                    "terminal_state": if index < successful { "SUCCESS" } else { "FAILED" },
                    "mutating": false,
                    "turn_completed": true,
                    "error_code": if index < successful { serde_json::Value::Null } else { serde_json::json!("TOOL_EXECUTION_FAILED") }
                })).collect::<Vec<_>>(),
                "provider_updates": (0..total).map(|index| serde_json::json!({
                    "tool_invocation_id": format!("oti-{index}"),
                    "provider_tool_call_id": format!("call-{index}"),
                    "correlation_state": "OBSERVED"
                })).collect::<Vec<_>>(),
                "omitted_count": 0,
                "provider_tool_names_omitted": 0
            })
        };

        let clean = audit(2, 2, 0, 0);
        assert!(durable_tool_call_audit_is_strict(&clean, 2, 2, 0));

        let no_tool_invocations = serde_json::json!({
            "schema_version": 2,
            "summary": {
                "total": 0,
                "callback_count": 0,
                "provider_notification_count": 0,
                "successful": 0,
                "unsuccessful": 0,
                "unmatched_provider_calls": 0,
                "unmatched_callbacks": 0,
                "mutating": 0,
                "denied": 0,
                "mutating_unknown": 0
            },
            "correlation_capability": "NOT_EXERCISED",
            "entries": [],
            "provider_updates": [],
            "omitted_count": 0,
            "provider_tool_names_omitted": 0
        });
        assert!(durable_tool_call_audit_is_strict(
            &no_tool_invocations,
            0,
            0,
            0
        ));

        let mut empty_but_unsupported = no_tool_invocations.clone();
        empty_but_unsupported["correlation_capability"] = serde_json::json!("UNSUPPORTED");
        assert!(!durable_tool_call_audit_is_strict(
            &empty_but_unsupported,
            0,
            0,
            0
        ));

        let notification_only = serde_json::json!({
            "schema_version": 2,
            "summary": {
                "total": 1,
                "callback_count": 0,
                "provider_notification_count": 1,
                "successful": 0,
                "unsuccessful": 1,
                "unmatched_provider_calls": 1,
                "unmatched_callbacks": 0,
                "mutating": 0,
                "denied": 0,
                "mutating_unknown": 0
            },
            "correlation_capability": "PARTIAL",
            "entries": [{
                "provider_update_correlation": "UNMATCHED",
                "terminal_state": "UNRESOLVED"
            }],
            "provider_updates": [{
                "tool_invocation_id": "oti-notification",
                "provider_tool_call_id": "provider-notification",
                "correlation_state": "OBSERVED"
            }],
            "omitted_count": 0,
            "provider_tool_names_omitted": 0
        });
        assert!(!durable_tool_call_audit_is_strict(
            &notification_only,
            0,
            0,
            0
        ));

        let mut legacy_schema = clean.clone();
        legacy_schema["schema_version"] = serde_json::json!(1);
        assert!(!durable_tool_call_audit_is_strict(&legacy_schema, 2, 2, 0));

        let mut reused_invocation = clean.clone();
        reused_invocation["entries"][1]["tool_invocation_id"] =
            reused_invocation["entries"][0]["tool_invocation_id"].clone();
        assert!(!durable_tool_call_audit_is_strict(
            &reused_invocation,
            2,
            2,
            0
        ));

        let mut reused_callback = clean.clone();
        reused_callback["entries"][1]["callback_request_id"] =
            reused_callback["entries"][0]["callback_request_id"].clone();
        assert!(!durable_tool_call_audit_is_strict(
            &reused_callback,
            2,
            2,
            0
        ));

        let mut malformed_id = clean.clone();
        malformed_id["entries"][0]["provider_tool_call_id"] =
            serde_json::json!("/private/provider/payload");
        assert!(!durable_tool_call_audit_is_strict(&malformed_id, 2, 2, 0));

        let mut duplicate_update = clean.clone();
        duplicate_update["provider_updates"][1]["tool_invocation_id"] =
            duplicate_update["provider_updates"][0]["tool_invocation_id"].clone();
        duplicate_update["provider_updates"][1]["provider_tool_call_id"] =
            duplicate_update["provider_updates"][0]["provider_tool_call_id"].clone();
        assert!(!durable_tool_call_audit_is_strict(
            &duplicate_update,
            2,
            2,
            0
        ));

        let mut extraneous_update = clean.clone();
        extraneous_update["provider_updates"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "tool_invocation_id": "oti-extra",
                "provider_tool_call_id": "call-extra",
                "correlation_state": "OBSERVED"
            }));
        assert!(!durable_tool_call_audit_is_strict(
            &extraneous_update,
            2,
            2,
            0
        ));

        let mut missing_update = clean.clone();
        missing_update["provider_updates"]
            .as_array_mut()
            .unwrap()
            .pop();
        assert!(!durable_tool_call_audit_is_strict(&missing_update, 2, 2, 0));

        let mut ambiguous_update = clean.clone();
        ambiguous_update["entries"][0]["provider_update_correlation"] =
            serde_json::json!("AMBIGUOUS");
        ambiguous_update["entries"][0]["provider_update_title_class"] =
            serde_json::json!("update_not_observed");
        assert!(!durable_tool_call_audit_is_strict(
            &ambiguous_update,
            2,
            2,
            0
        ));

        for field in ["advertised_to_provider", "role_allowed"] {
            let mut missing_required_authority = clean.clone();
            missing_required_authority["entries"][0]
                .as_object_mut()
                .unwrap()
                .remove(field);
            assert!(!durable_tool_call_audit_is_strict(
                &missing_required_authority,
                2,
                2,
                0
            ));

            let mut false_required_authority = clean.clone();
            false_required_authority["entries"][0][field] = serde_json::json!(false);
            assert!(!durable_tool_call_audit_is_strict(
                &false_required_authority,
                2,
                2,
                0
            ));
        }

        let mut mismatched_update = clean.clone();
        mismatched_update["entries"][0]["provider_tool_name"] =
            serde_json::json!("fs/write_text_file");
        mismatched_update["entries"][0]["provider_name_mapping"] = serde_json::json!("MISMATCH");
        assert!(!durable_tool_call_audit_is_strict(
            &mismatched_update,
            2,
            2,
            0
        ));

        let mut malformed_method = clean.clone();
        malformed_method["entries"][0]["provider_tool_name"] =
            serde_json::json!(" fs/read_text_file ");
        assert!(!durable_tool_call_audit_is_strict(
            &malformed_method,
            2,
            2,
            0
        ));

        let unmatched = audit(4, 2, 2, 2);
        assert!(!durable_tool_call_audit_is_strict(&unmatched, 2, 2, 0));

        let mut equal_cardinality = audit(2, 2, 0, 0);
        equal_cardinality["entries"][0]["provider_tool_name"] =
            serde_json::json!("fs/read_text_file");
        equal_cardinality["entries"][0]["canonical_tool_name"] =
            serde_json::json!("fs.read_text_file");
        equal_cardinality["entries"][0]["provider_update_correlation"] =
            serde_json::json!("AMBIGUOUS");
        equal_cardinality["entries"][1]["provider_tool_name"] =
            serde_json::json!("fs/write_text_file");
        equal_cardinality["entries"][1]["canonical_tool_name"] =
            serde_json::json!("fs.write_text_file");
        equal_cardinality["entries"][1]["provider_update_correlation"] =
            serde_json::json!("AMBIGUOUS");
        assert!(!successful_tool_audit_rows_are_correlated(
            equal_cardinality["entries"].as_array().unwrap(),
            equal_cardinality["provider_updates"].as_array().unwrap(),
            Some(0)
        ));
        equal_cardinality["entries"]
            .as_array_mut()
            .unwrap()
            .extend([
                serde_json::json!({
                    "sequence": 3,
                    "provider_tool_name": "unknown",
                    "provider_name_mapping": "UNMATCHED",
                    "provider_update_correlation": "UNMATCHED",
                    "provider_update_title_class": "non_empty_string",
                    "provider_update_tool_kind": "edit",
                    "provider_update_status": "in_progress",
                    "provider_tool_call_id_shape": "string",
                    "canonical_tool_name": "unknown",
                    "advertised_to_provider": null,
                    "role_allowed": null,
                    "outcome": "EXECUTION_FAILURE",
                    "error_code": "PROVIDER_CALLBACK_UNRESOLVED"
                }),
                serde_json::json!({
                    "sequence": 4,
                    "provider_tool_name": "unknown",
                    "provider_name_mapping": "UNMATCHED",
                    "provider_update_correlation": "UNMATCHED",
                    "provider_update_title_class": "non_empty_string",
                    "provider_update_tool_kind": "read",
                    "provider_update_status": "in_progress",
                    "provider_tool_call_id_shape": "string",
                    "canonical_tool_name": "unknown",
                    "advertised_to_provider": null,
                    "role_allowed": null,
                    "outcome": "EXECUTION_FAILURE",
                    "error_code": "PROVIDER_CALLBACK_UNRESOLVED"
                }),
            ]);
        equal_cardinality["summary"]["total"] = serde_json::json!(4);
        equal_cardinality["summary"]["successful"] = serde_json::json!(2);
        equal_cardinality["summary"]["unsuccessful"] = serde_json::json!(2);
        equal_cardinality["summary"]["unmatched_provider_calls"] = serde_json::json!(2);
        assert!(!durable_tool_call_audit_is_strict(
            &equal_cardinality,
            4,
            2,
            2
        ));

        let mismatched_counter = audit(2, 1, 1, 0);
        assert!(!durable_tool_call_audit_is_strict(
            &mismatched_counter,
            2,
            2,
            0
        ));

        let contradictory = audit(2, 2, 0, 0);
        assert!(!durable_tool_call_audit_is_strict(&contradictory, 2, 2, 1));

        let mut omitted = clean.clone();
        omitted["omitted_count"] = serde_json::json!(1);
        assert!(!durable_tool_call_audit_is_strict(&omitted, 2, 2, 0));

        let mut denied = clean.clone();
        denied["summary"]["denied"] = serde_json::json!(1);
        assert!(!durable_tool_call_audit_is_strict(&denied, 2, 2, 0));

        let mut unknown_mutation = clean.clone();
        unknown_mutation["summary"]["mutating_unknown"] = serde_json::json!(1);
        assert!(!durable_tool_call_audit_is_strict(
            &unknown_mutation,
            2,
            2,
            0
        ));

        let mut failed_entry = clean;
        failed_entry["entries"][0]["outcome"] = serde_json::json!("EXECUTION_FAILURE");
        assert!(!durable_tool_call_audit_is_strict(&failed_entry, 2, 2, 0));

        let mut missing_mutating = audit(2, 2, 0, 0);
        missing_mutating["entries"][0]
            .as_object_mut()
            .unwrap()
            .remove("mutating");
        assert!(!durable_tool_call_audit_is_strict(
            &missing_mutating,
            2,
            2,
            0
        ));

        let mut inconsistent_mutating = audit(2, 2, 0, 0);
        inconsistent_mutating["entries"][0]["mutating"] = serde_json::json!(true);
        assert!(!durable_tool_call_audit_is_strict(
            &inconsistent_mutating,
            2,
            2,
            0
        ));
        inconsistent_mutating["summary"]["mutating"] = serde_json::json!(1);
        assert!(durable_tool_call_audit_is_strict(
            &inconsistent_mutating,
            2,
            2,
            0
        ));

        let mut incomplete_turn = audit(2, 2, 0, 0);
        incomplete_turn["entries"][0]["turn_completed"] = serde_json::json!(false);
        assert!(!durable_tool_call_audit_is_strict(
            &incomplete_turn,
            2,
            2,
            0
        ));

        let mut duplicate_sequence = audit(2, 2, 0, 0);
        duplicate_sequence["entries"][1]["sequence"] =
            duplicate_sequence["entries"][0]["sequence"].clone();
        assert!(!durable_tool_call_audit_is_strict(
            &duplicate_sequence,
            2,
            2,
            0
        ));

        let mut mismatched_notification_count = audit(2, 2, 0, 0);
        mismatched_notification_count["summary"]["provider_notification_count"] =
            serde_json::json!(1);
        assert!(!durable_tool_call_audit_is_strict(
            &mismatched_notification_count,
            2,
            2,
            0
        ));
    }

    #[test]
    fn fixed_live_policies_require_the_same_real_check_at_every_tier() {
        let policies = live_cli_qualification_policies();
        assert_eq!(
            policies.verification.required_steps,
            vec!["candidate-contract".to_owned()]
        );
        assert_eq!(policies.verification.allowed_commands[0].executable, "sh");
        assert_eq!(
            policies.verification.allowed_commands[0].args_prefix,
            vec!["test.sh".to_owned()]
        );
        assert_eq!(
            policies.selection.checks.len(),
            1,
            "the qualification selection is fixed to one authoritative check"
        );
        let check = &policies.selection.checks[0];
        assert!(check.required && check.always_run);
        assert_eq!(check.check_id, "candidate-contract");
        assert_eq!(
            check.command.as_ref().unwrap(),
            &vec!["sh".to_owned(), "test.sh".to_owned()]
        );
        assert_eq!(
            check.tiers,
            [
                orbit::regression_strategy::VerificationTier::Fast,
                orbit::regression_strategy::VerificationTier::Standard,
                orbit::regression_strategy::VerificationTier::Full,
            ]
        );
        assert_eq!(
            policies.regression.selection_policy_digest.as_deref(),
            Some(policies.selection.digest().as_str())
        );
        assert_eq!(
            policies.regression.fallback_behavior,
            orbit::regression_strategy::RegressionFallbackBehavior::FailClosed
        );
    }

    #[test]
    fn local_image_identity_requires_sha256_and_is_pinned_to_rootless_profile() {
        let digest = "a".repeat(64);
        let environment = qualification_environment_from_image_id(&format!("sha256:{digest}"))
            .expect("valid local Podman image ID");
        assert_eq!(
            environment.runtime_image.as_deref(),
            Some(LIVE_QUALIFICATION_VERIFICATION_IMAGE)
        );
        assert_eq!(
            environment.runtime_image_digest.as_deref(),
            Some(format!("sha256:{digest}").as_str())
        );
        assert_eq!(environment.execution_profile, "sandboxed-container");
        assert_eq!(environment.isolation, "rootless-podman");
        assert_eq!(environment.oci_runtime.as_deref(), Some("podman"));
        assert!(qualification_environment_from_image_id("not-a-digest").is_err());
    }

    #[test]
    fn quota_report_preserves_reset_aware_selection_facts() {
        let facts = reset_aware_selection_evidence(
            "reset-aware rank=1; known_weekly_reset; quota_snapshot_freshness=FRESH; availability=Ready; 5h_remaining=82.0%; 7d_remaining=61.0%; 7d_reset_at_ms=1780000000000; provider_preference_rank=0; tie_break=provider_preference_then_stable_account_id; rejected=[codex:private-account:quota_exhausted(5h=unknown,7d=unknown,7d_reset=unknown),antigravity:another-private-account:auth_failed(5h=unknown,7d=unknown,7d_reset=unknown)]",
        );
        assert_eq!(facts["ranking"], "reset-aware");
        assert_eq!(facts["rank"], "1");
        assert_eq!(facts["weekly_reset_rank"], "known_weekly_reset");
        assert_eq!(facts["quota_snapshot_freshness"], "FRESH");
        assert_eq!(
            facts["selection_reason"]["quota_snapshot_freshness"],
            "FRESH"
        );
        assert_eq!(facts["availability"], "Ready");
        assert_eq!(facts["five_hour_remaining"], "82.0%");
        assert_eq!(facts["seven_day_remaining"], "61.0%");
        assert_eq!(facts["seven_day_reset_at_ms"], "1780000000000");
        assert_eq!(facts["five_hour_reset_at_ms"], serde_json::Value::Null);
        assert_eq!(facts["rejected_candidate_summary_items"], 2);
        let projected = serde_json::to_string(&facts).unwrap();
        assert!(!projected.contains("private-account"));
        assert!(!projected.contains("another-private-account"));
    }

    #[test]
    fn quota_report_distinguishes_stale_and_absent_snapshots_without_quota_facts() {
        for (freshness, availability) in [("STALE", "Unknown"), ("ABSENT", "Unknown")] {
            let reason = format!(
                "reset-aware rank=1; weekly_reset_unknown_or_not_applicable; quota_snapshot_freshness={freshness}; availability={availability}; 5h_remaining=unknown; 7d_remaining=unknown; 7d_reset_at_ms=unknown; provider_preference_rank=0; tie_break=provider_preference_then_stable_account_id; rejected=[]"
            );
            let evidence = reset_aware_selection_evidence(&reason);
            assert_eq!(evidence["quota_snapshot_freshness"], freshness);
            assert_eq!(evidence["availability"], "Unknown");
            assert_eq!(
                evidence["weekly_reset_rank"],
                "weekly_reset_unknown_or_not_applicable"
            );
            assert_eq!(evidence["five_hour_remaining"], "unknown");
            assert_eq!(evidence["seven_day_remaining"], "unknown");
            assert_eq!(evidence["seven_day_reset_at_ms"], "unknown");
        }

        let fresh_without_known_weekly_facts = reset_aware_selection_evidence(
            "reset-aware rank=1; weekly_reset_unknown_or_not_applicable; quota_snapshot_freshness=FRESH; availability=Unknown; 5h_remaining=unknown; 7d_remaining=unknown; 7d_reset_at_ms=unknown; provider_preference_rank=0; tie_break=provider_preference_then_stable_account_id; rejected=[]",
        );
        assert_eq!(
            fresh_without_known_weekly_facts["quota_snapshot_freshness"],
            "FRESH"
        );
        assert_eq!(fresh_without_known_weekly_facts["availability"], "Unknown");
        assert_eq!(
            fresh_without_known_weekly_facts["weekly_reset_rank"],
            "weekly_reset_unknown_or_not_applicable"
        );

        let unsupported = reset_aware_selection_evidence(
            "reset-aware rank=1; weekly_reset_unknown_or_not_applicable; quota_snapshot_freshness=provider-payload; availability=Unknown; 5h_remaining=unknown; 7d_remaining=unknown; 7d_reset_at_ms=unknown; provider_preference_rank=0; tie_break=provider_preference_then_stable_account_id; rejected=[]",
        );
        assert_eq!(
            unsupported["quota_snapshot_freshness"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn tool_telemetry_is_bounded_to_known_names_and_canonical_mutations() {
        let counts = bounded_tool_counts(&serde_json::json!({
            "write_file": 2,
            "fs.write_text_file": 2,
            "fs.read_text_file": 3,
            "untrusted-secret-shaped-tool-name": 99
        }));
        assert_eq!(counts.as_object().unwrap().len(), 3);
        assert_eq!(
            tool_count(&counts, &["fs.write_text_file", "fs.edit_file"]),
            2
        );
        assert!(counts.get("untrusted-secret-shaped-tool-name").is_none());
    }

    #[test]
    fn quota_percentages_are_bounded_before_reporting() {
        assert_eq!(safe_quota_percent(Some(42.5)), Some(42.5));
        assert_eq!(safe_quota_percent(Some(f64::NAN)), None);
        assert_eq!(safe_quota_percent(Some(101.0)), None);
    }

    #[test]
    fn qualification_repository_must_be_clean_and_narrowly_shaped_under_temp() {
        let temp = tempfile::tempdir().expect("temporary root");
        let repository = clean_temp_git_repository(temp.path());
        let validated =
            validate_live_qualification_repository(&repository).expect("valid fixture repo");
        assert_eq!(validated.path, repository.canonicalize().unwrap());
        assert_eq!(validated.base_revision.len(), 40);

        fs::write(repository.join("test.sh"), "echo unsafe\n").unwrap();
        assert!(validate_live_qualification_repository(&repository).is_err());
    }

    #[test]
    fn candidate_contract_accepts_only_readme_mutation_with_fixed_harness() {
        let temp = tempfile::tempdir().expect("temporary root");
        let repository_path = clean_temp_git_repository(temp.path());
        let repository =
            validate_live_qualification_repository(&repository_path).expect("fixture repo");
        fs::write(
            repository.path.join("README.md"),
            format!("{LIVE_QUALIFICATION_README_CONTENT}\n"),
        )
        .unwrap();
        let valid = candidate_contract_evidence(&repository);
        assert_eq!(valid["only_expected_readme_change"], true);
        assert_eq!(valid["fixed_harness_unchanged"], true);

        fs::write(repository.path.join(".env"), "not part of the candidate\n").unwrap();
        let extra_file = candidate_contract_evidence(&repository);
        assert_eq!(extra_file["only_expected_readme_change"], false);
        fs::remove_file(repository.path.join(".env")).unwrap();

        fs::write(repository.path.join("test.sh"), "echo forged pass\n").unwrap();
        let forged = candidate_contract_evidence(&repository);
        assert_eq!(forged["only_expected_readme_change"], false);
        assert_eq!(forged["fixed_harness_unchanged"], false);
    }

    #[test]
    fn qualification_repository_rejects_dirty_non_git_and_source_roots() {
        let temp = tempfile::tempdir().expect("temporary root");
        let repository = clean_temp_git_repository(temp.path());
        fs::write(repository.join("README.md"), "modified after commit\n").unwrap();
        assert!(validate_live_qualification_repository(&repository).is_err());

        let non_git = temp.path().join("plain");
        fs::create_dir(&non_git).unwrap();
        fs::write(non_git.join("README.md"), "plain\n").unwrap();
        assert!(validate_live_qualification_repository(&non_git).is_err());

        assert!(
            validate_live_qualification_repository(Path::new(env!("CARGO_MANIFEST_DIR"))).is_err()
        );
        assert!(validate_live_qualification_repository(temp.path()).is_err());
    }

    #[test]
    fn qualification_repository_rejects_custom_git_hooks() {
        let temp = tempfile::tempdir().expect("temporary root");
        let repository = clean_temp_git_repository(temp.path());
        fs::write(
            repository.join(".git/hooks/pre-commit"),
            "#!/bin/sh\nexit 0\n",
        )
        .unwrap();
        assert!(validate_live_qualification_repository(&repository).is_err());
    }

    #[test]
    fn qualification_git_inspection_ignores_executable_config_and_rejects_its_shape() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temporary root");
        let repository = clean_temp_git_repository(temp.path());
        let fsmonitor_marker = temp.path().join("fsmonitor-ran");
        let diff_marker = temp.path().join("external-diff-ran");
        let fsmonitor_script = temp.path().join("fsmonitor.sh");
        let diff_script = temp.path().join("external-diff.sh");
        fs::write(
            &fsmonitor_script,
            format!(
                "#!/bin/sh\ntouch '{}'\nprintf 'token'\n",
                fsmonitor_marker.display()
            ),
        )
        .unwrap();
        fs::write(
            &diff_script,
            format!("#!/bin/sh\ntouch '{}'\n", diff_marker.display()),
        )
        .unwrap();
        fs::set_permissions(&fsmonitor_script, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&diff_script, fs::Permissions::from_mode(0o700)).unwrap();
        let set_config = |key: &str, value: &Path| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&repository)
                .args(["config", "--local", key])
                .arg(value)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .expect("set fixture Git config");
            assert!(output.status.success());
        };
        set_config("core.fsmonitor", &fsmonitor_script);
        set_config("diff.external", &diff_script);

        let keys = local_git_config_keys_for_qualification(&repository).unwrap();
        assert!(keys.iter().any(|key| key == "core.fsmonitor"));
        assert!(keys.iter().any(|key| key == "diff.external"));
        assert!(validate_local_git_config_for_qualification(&repository).is_err());
        let status = read_only_qualification_git_command(&repository, &["status", "--short"])
            .output()
            .expect("run hardened status inspection");
        assert!(status.status.success());
        let diff = read_only_qualification_git_command(
            &repository,
            &["diff", "--name-only", "HEAD", "--"],
        )
        .output()
        .expect("run hardened diff inspection");
        assert!(diff.status.success());
        assert!(!fsmonitor_marker.exists());
        assert!(!diff_marker.exists());
    }

    #[test]
    fn qualification_git_config_include_is_rejected_without_loading_external_file() {
        let temp = tempfile::tempdir().expect("temporary root");
        let repository = clean_temp_git_repository(temp.path());
        let marker = temp.path().join("included-config-executed");
        let script = temp.path().join("included-helper.sh");
        fs::write(
            &script,
            format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let included_config = temp.path().join("external.gitconfig");
        fs::write(
            &included_config,
            format!("[diff \"external\"]\ntextconv = {}\n", script.display()),
        )
        .unwrap();
        let mut local_config = fs::read_to_string(repository.join(".git/config")).unwrap();
        local_config.push_str(&format!(
            "\n[include]\npath = {}\n",
            included_config.display()
        ));
        fs::write(repository.join(".git/config"), local_config).unwrap();

        let keys = local_git_config_keys_for_qualification(&repository).unwrap();
        assert!(keys.iter().any(|key| key == "include.path"));
        assert!(!keys.iter().any(|key| key == "diff.external.textconv"));
        assert!(validate_local_git_config_for_qualification(&repository).is_err());
        assert!(!marker.exists());
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
