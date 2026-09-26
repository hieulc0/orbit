//! Phase B1: Isolated Command Execution and Verification Evidence.
//!
//! Provides the verification substrate for Orbit:
//! - Orbit independently executes verification commands inside the Attempt's isolated workspace.
//! - Records durable evidence tied to the exact [`WorkspaceState`].
//! - Enforces that an agent's claim is never verification evidence.
//! - Invalidates verification freshness when the workspace is mutated.
//! - Bounded command execution with timeout, process-group cleanup, and output limits.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use std::{collections::BTreeMap, path::Path, time::Duration};
use tokio::time::Instant;

pub const DEFAULT_STEP_TIMEOUT_SECONDS: u64 = 300;
pub const MAX_INLINE_OUTPUT_BYTES: usize = 16 * 1024; // 16 KiB bounded preview in DB
pub const MAX_STREAM_OUTPUT_BYTES: usize = 8 * 1024 * 1024; // 8 MiB per stream

/// Unique immutable identity of a workspace state.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkspaceState {
    pub state_id: String,
    pub baseline_revision: String,
    pub head_revision: String,
    pub diff_sha256: Option<String>,
}

impl WorkspaceState {
    pub fn from_snapshot(snapshot: &crate::continuation::WorkspaceSnapshot) -> Self {
        let diff_hash = snapshot.diff_sha256.as_deref().unwrap_or("clean");
        let raw = format!(
            "{}:{}:{}",
            snapshot.baseline_revision, snapshot.head_revision, diff_hash
        );
        let state_id = format!("ws-{}", crate::model::digest(raw.as_bytes()));
        Self {
            state_id,
            baseline_revision: snapshot.baseline_revision.clone(),
            head_revision: snapshot.head_revision.clone(),
            diff_sha256: snapshot.diff_sha256.clone(),
        }
    }

    pub fn compute_from_parts(baseline: &str, head: &str, diff_sha256: Option<&str>) -> Self {
        let diff_hash = diff_sha256.unwrap_or("clean");
        let raw = format!("{}:{}:{}", baseline, head, diff_hash);
        let state_id = format!("ws-{}", crate::model::digest(raw.as_bytes()));
        Self {
            state_id,
            baseline_revision: baseline.to_string(),
            head_revision: head.to_string(),
            diff_sha256: diff_sha256.map(|s| s.to_string()),
        }
    }
}

/// Verification step execution kind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStepKind {
    Command,
}

/// A single step in a verification plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationStep {
    pub id: String,
    pub name: String,
    pub kind: VerificationStepKind,
    pub argv: Vec<String>,
    #[serde(default = "default_cwd")]
    pub cwd: String,
    #[serde(default)]
    pub env_keys: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_true")]
    pub required: bool,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

fn default_cwd() -> String {
    ".".to_string()
}

fn default_timeout() -> u64 {
    DEFAULT_STEP_TIMEOUT_SECONDS
}

fn default_true() -> bool {
    true
}

impl VerificationStep {
    pub fn new_command(id: impl Into<String>, name: impl Into<String>, argv: Vec<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            kind: VerificationStepKind::Command,
            argv,
            cwd: default_cwd(),
            env_keys: Vec::new(),
            env: BTreeMap::new(),
            timeout_seconds: default_timeout(),
            required: true,
            depends_on: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(!self.id.trim().is_empty(), "step id required");
        ensure!(!self.name.trim().is_empty(), "step name required");
        ensure!(!self.argv.is_empty(), "step argv cannot be empty");
        ensure!(self.timeout_seconds > 0, "timeout_seconds must be > 0");
        Ok(())
    }
}

/// Versioned, immutable verification plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationPlan {
    pub id: String,
    pub version: u32,
    pub name: String,
    pub steps: Vec<VerificationStep>,
}

impl VerificationPlan {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        steps: Vec<VerificationStep>,
    ) -> Self {
        Self {
            id: id.into(),
            version: 1,
            name: name.into(),
            steps,
        }
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(!self.id.trim().is_empty(), "plan id required");
        ensure!(!self.name.trim().is_empty(), "plan name required");
        ensure!(
            !self.steps.is_empty(),
            "plan must contain at least one step"
        );
        ensure!(
            self.steps.iter().any(|s| s.required),
            "plan must contain at least one required verification step; absence of tests is not verification"
        );
        let mut ids = std::collections::HashSet::new();
        for step in &self.steps {
            step.validate()?;
            ensure!(ids.insert(&step.id), "duplicate step id: {}", step.id);
        }
        Ok(())
    }
}

/// Status of an individual verification step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum VerificationStepStatus {
    Pending,
    Running,
    Passed,
    Failed,
    Error,
    TimedOut,
    Cancelled,
    Skipped,
}

impl std::fmt::Display for VerificationStepStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "PENDING"),
            Self::Running => write!(f, "RUNNING"),
            Self::Passed => write!(f, "PASSED"),
            Self::Failed => write!(f, "FAILED"),
            Self::Error => write!(f, "ERROR"),
            Self::TimedOut => write!(f, "TIMED_OUT"),
            Self::Cancelled => write!(f, "CANCELLED"),
            Self::Skipped => write!(f, "SKIPPED"),
        }
    }
}

/// Overall outcome of a verification run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum VerificationRunResult {
    Passed,
    Failed,
    Error,
    TimedOut,
    Cancelled,
}

impl std::fmt::Display for VerificationRunResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Passed => write!(f, "PASSED"),
            Self::Failed => write!(f, "FAILED"),
            Self::Error => write!(f, "ERROR"),
            Self::TimedOut => write!(f, "TIMED_OUT"),
            Self::Cancelled => write!(f, "CANCELLED"),
        }
    }
}

/// Environment identity metadata captured for a verification run.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentIdentity {
    pub execution_profile: String,
    pub isolation: String,
    pub runtime_image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_image_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oci_runtime: Option<String>,
    pub architecture: String,
    pub os: String,
    pub orbit_version: String,
}

/// Durable record of a verification step execution.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VerificationStepRun {
    pub id: String,
    pub verification_run_id: String,
    pub step_id: String,
    pub step_name: String,
    pub status: VerificationStepStatus,
    pub required: bool,
    pub exit_code: Option<i32>,
    pub started_at_ms: i64,
    pub finished_at_ms: Option<i64>,
    pub duration_ms: Option<i64>,
    pub stdout_preview: Option<String>,
    pub stdout_truncated: bool,
    pub stdout_bytes: u64,
    pub stdout_artifact_id: Option<String>,
    pub stderr_preview: Option<String>,
    pub stderr_truncated: bool,
    pub stderr_bytes: u64,
    pub stderr_artifact_id: Option<String>,
    pub artifacts: Vec<crate::model::Artifact>,
    pub error_message: Option<String>,
}

/// Durable record of an entire verification run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VerificationRun {
    pub id: String,
    pub attempt_id: String,
    pub workspace_state_id: String,
    pub plan_id: String,
    pub plan_version: u32,
    pub plan_snapshot: VerificationPlan,
    pub status: VerificationStepStatus,
    pub environment_identity: EnvironmentIdentity,
    pub started_at_ms: i64,
    pub finished_at_ms: Option<i64>,
    pub overall_result: Option<VerificationRunResult>,
    pub step_runs: Vec<VerificationStepRun>,
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Process group guard ensuring child process trees are terminated on drop or cancellation.
pub struct ScopedProcessGroup(pub u32);

impl Drop for ScopedProcessGroup {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.0 as i32), libc::SIGKILL);
        }
    }
}

/// Bounded output capture result.
#[derive(Debug)]
pub struct CommandOutputCapture {
    pub exit_code: Option<i32>,
    pub stdout_bytes: Vec<u8>,
    pub stdout_total_len: u64,
    pub stdout_truncated: bool,
    pub stderr_bytes: Vec<u8>,
    pub stderr_total_len: u64,
    pub stderr_truncated: bool,
    pub timed_out: bool,
    pub cancelled: bool,
    pub duration_ms: i64,
}

/// Execute a single verification command with bounded output, strict timeout, and process-tree cleanup.
pub async fn execute_verification_command(
    step: &VerificationStep,
    workspace_dir: &Path,
    timeout_override: Option<Duration>,
    cancellation_token: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<CommandOutputCapture> {
    execute_verification_command_isolated(
        step,
        workspace_dir,
        timeout_override,
        cancellation_token,
        None,
    )
    .await
}

pub async fn execute_verification_command_isolated(
    step: &VerificationStep,
    workspace_dir: &Path,
    timeout_override: Option<Duration>,
    mut cancellation_token: Option<tokio::sync::watch::Receiver<bool>>,
    isolation_image: Option<&str>,
) -> Result<CommandOutputCapture> {
    use std::process::Stdio;
    use tokio::{io::AsyncReadExt, process::Command};

    step.validate()?;
    let canonical_workspace = workspace_dir.canonicalize()?;
    let cwd = canonical_workspace.join(&step.cwd).canonicalize()?;
    ensure!(
        cwd.starts_with(&canonical_workspace),
        "command cwd escapes workspace"
    );

    let start_instant = Instant::now();
    let container_name = isolation_image.map(|_| format!("orbit-vstep-{}", crate::model::id()));

    let mut cmd = if let Some(image) = isolation_image {
        let mut c = Command::new("podman");
        let name = container_name.as_ref().unwrap();
        let rel_cwd = cwd
            .strip_prefix(&canonical_workspace)
            .unwrap_or(Path::new("."))
            .to_string_lossy();
        let cont_cwd = if rel_cwd.is_empty() || rel_cwd == "." {
            "/workspace".to_string()
        } else {
            format!("/workspace/{}", rel_cwd)
        };

        c.args([
            "--remote=false",
            "--cgroup-manager=cgroupfs",
            "run",
            "--rm",
            "--name",
            name,
            "--network=none",
            "--read-only",
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--pids-limit=128",
            "--init",
            "--log-driver=none",
            "--tmpfs",
            "/tmp:rw,nosuid,nodev,size=67108864",
            "--mount",
            &format!(
                "type=bind,src={},dst=/workspace",
                canonical_workspace.display()
            ),
            "--workdir",
            &cont_cwd,
            "--env",
            "HOME=/tmp",
            "--env",
            "GIT_CONFIG_NOSYSTEM=1",
            "--env",
            "GIT_CONFIG_GLOBAL=/dev/null",
        ]);

        for (k, v) in &step.env {
            c.arg("--env").arg(format!("{}={}", k, v));
        }

        c.arg(image);
        // Translate any argv leading with canonical workspace to /workspace
        let canonical_str = canonical_workspace.to_string_lossy();
        for arg in &step.argv {
            if arg.starts_with(canonical_str.as_ref()) {
                let suffix = &arg[canonical_str.len()..];
                let cont_arg = format!("/workspace{}", suffix);
                c.arg(cont_arg);
            } else {
                c.arg(arg);
            }
        }
        c
    } else {
        let mut c = Command::new(&step.argv[0]);
        c.args(&step.argv[1..])
            .current_dir(&cwd)
            .env_clear()
            .envs(&step.env)
            .env(
                "PATH",
                std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into()),
            )
            .env(
                "HOME",
                std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()),
            );
        c
    };

    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true);

    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("cannot spawn verification command: {:?}", step.argv))?;

    let child_pid = child
        .id()
        .context("verification command child has no pid")?;
    let _group_guard = ScopedProcessGroup(child_pid);

    let mut stdout_pipe = child.stdout.take().context("missing stdout pipe")?;
    let mut stderr_pipe = child.stderr.take().context("missing stderr pipe")?;

    let timeout_duration =
        timeout_override.unwrap_or_else(|| Duration::from_secs(step.timeout_seconds));

    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();
    let mut stdout_total = 0u64;
    let mut stderr_total = 0u64;

    let mut stdout_chunk = [0u8; 8192];
    let mut stderr_chunk = [0u8; 8192];

    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut child_exit_code = None;

    let deadline = Instant::now() + timeout_duration;
    let mut timed_out = false;
    let mut cancelled = false;

    loop {
        if cancellation_token
            .as_ref()
            .is_some_and(|token| *token.borrow())
        {
            cancelled = true;
            break;
        }

        tokio::select! {
            changed = async {
                if let Some(ref mut token) = cancellation_token {
                    token.changed().await.ok();
                    *token.borrow()
                } else {
                    std::future::pending::<bool>().await
                }
            } => {
                if changed {
                    cancelled = true;
                    break;
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                timed_out = true;
                break;
            }
            res = child.wait(), if child_exit_code.is_none() => {
                let status = res?;
                child_exit_code = status.code();
                if stdout_done && stderr_done {
                    break;
                }
            }
            n = stdout_pipe.read(&mut stdout_chunk), if !stdout_done => {
                let bytes_read = n?;
                if bytes_read == 0 {
                    stdout_done = true;
                } else {
                    stdout_total += bytes_read as u64;
                    if stdout_buf.len() < MAX_STREAM_OUTPUT_BYTES {
                        let take = bytes_read.min(MAX_STREAM_OUTPUT_BYTES - stdout_buf.len());
                        stdout_buf.extend_from_slice(&stdout_chunk[..take]);
                    }
                }
            }
            n = stderr_pipe.read(&mut stderr_chunk), if !stderr_done => {
                let bytes_read = n?;
                if bytes_read == 0 {
                    stderr_done = true;
                } else {
                    stderr_total += bytes_read as u64;
                    if stderr_buf.len() < MAX_STREAM_OUTPUT_BYTES {
                        let take = bytes_read.min(MAX_STREAM_OUTPUT_BYTES - stderr_buf.len());
                        stderr_buf.extend_from_slice(&stderr_chunk[..take]);
                    }
                }
            }
        }

        if child_exit_code.is_some() && stdout_done && stderr_done {
            break;
        }
    }

    if timed_out || cancelled {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(child_pid as i32), libc::SIGKILL);
        }
        if let Some(ref cname) = container_name {
            let mut stop_cmd = tokio::process::Command::new("podman");
            stop_cmd.args([
                "--remote=false",
                "--cgroup-manager=cgroupfs",
                "rm",
                "-f",
                cname,
            ]);
            let _ = stop_cmd.output().await;
        }
        let _ = child.wait().await;
        if cancelled {
            bail!("verification command cancelled");
        }
    }

    let duration_ms = start_instant.elapsed().as_millis() as i64;
    let stdout_truncated = stdout_total > MAX_STREAM_OUTPUT_BYTES as u64;
    let stderr_truncated = stderr_total > MAX_STREAM_OUTPUT_BYTES as u64;

    Ok(CommandOutputCapture {
        exit_code: child_exit_code,
        stdout_bytes: stdout_buf,
        stdout_total_len: stdout_total,
        stdout_truncated,
        stderr_bytes: stderr_buf,
        stderr_total_len: stderr_total,
        stderr_truncated,
        timed_out,
        cancelled,
        duration_ms,
    })
}

/// Durable verification store for PostgreSQL.
#[derive(Clone)]
pub struct VerificationStore {
    pool: PgPool,
}

impl VerificationStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Save or update a verification plan.
    pub async fn save_plan(&self, plan: &VerificationPlan) -> Result<()> {
        plan.validate()?;
        sqlx::query(
            r#"
            INSERT INTO orbit_verification_plans (id, version, name, definition)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (id) DO UPDATE SET
                version = EXCLUDED.version,
                name = EXCLUDED.name,
                definition = EXCLUDED.definition
            "#,
        )
        .bind(&plan.id)
        .bind(plan.version as i32)
        .bind(&plan.name)
        .bind(serde_json::to_value(plan)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Get a plan by ID.
    pub async fn get_plan(&self, plan_id: &str) -> Result<Option<VerificationPlan>> {
        let row = sqlx::query_scalar::<_, serde_json::Value>(
            "SELECT definition FROM orbit_verification_plans WHERE id = $1",
        )
        .bind(plan_id)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some(val) => Ok(Some(serde_json::from_value(val)?)),
            None => Ok(None),
        }
    }

    /// Create a new VerificationRun record in PENDING status.
    pub async fn create_run(
        &self,
        attempt_id: &str,
        workspace_state: &WorkspaceState,
        plan: &VerificationPlan,
        environment: EnvironmentIdentity,
    ) -> Result<VerificationRun> {
        plan.validate()?;
        let run_id = format!("vrun-{}", crate::model::id());
        let started_at_ms = now_millis();

        sqlx::query(
            r#"
            INSERT INTO orbit_verification_runs (
                id, attempt_id, workspace_state_id, plan_id, plan_version,
                plan_snapshot, status, environment_identity, started_at_ms
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            "#,
        )
        .bind(&run_id)
        .bind(attempt_id)
        .bind(&workspace_state.state_id)
        .bind(&plan.id)
        .bind(plan.version as i32)
        .bind(serde_json::to_value(plan)?)
        .bind("PENDING")
        .bind(serde_json::to_value(&environment)?)
        .bind(started_at_ms)
        .execute(&self.pool)
        .await?;

        Ok(VerificationRun {
            id: run_id,
            attempt_id: attempt_id.to_string(),
            workspace_state_id: workspace_state.state_id.clone(),
            plan_id: plan.id.clone(),
            plan_version: plan.version,
            plan_snapshot: plan.clone(),
            status: VerificationStepStatus::Pending,
            environment_identity: environment,
            started_at_ms,
            finished_at_ms: None,
            overall_result: None,
            step_runs: Vec::new(),
        })
    }

    /// Record the execution result of a single verification step.
    pub async fn record_step_run(&self, step_run: &VerificationStepRun) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO orbit_verification_step_runs (
                id, verification_run_id, step_id, step_name, status, required,
                exit_code, started_at_ms, finished_at_ms, duration_ms,
                stdout_preview, stdout_truncated, stdout_bytes, stdout_artifact_id,
                stderr_preview, stderr_truncated, stderr_bytes, stderr_artifact_id,
                artifacts, error_message
            )
            VALUES (
                $1, $2, $3, $4, $5, $6,
                $7, $8, $9, $10,
                $11, $12, $13, $14,
                $15, $16, $17, $18,
                $19, $20
            )
            ON CONFLICT (id) DO UPDATE SET
                status = EXCLUDED.status,
                exit_code = EXCLUDED.exit_code,
                finished_at_ms = EXCLUDED.finished_at_ms,
                duration_ms = EXCLUDED.duration_ms,
                stdout_preview = EXCLUDED.stdout_preview,
                stdout_truncated = EXCLUDED.stdout_truncated,
                stdout_bytes = EXCLUDED.stdout_bytes,
                stdout_artifact_id = EXCLUDED.stdout_artifact_id,
                stderr_preview = EXCLUDED.stderr_preview,
                stderr_truncated = EXCLUDED.stderr_truncated,
                stderr_bytes = EXCLUDED.stderr_bytes,
                stderr_artifact_id = EXCLUDED.stderr_artifact_id,
                artifacts = EXCLUDED.artifacts,
                error_message = EXCLUDED.error_message
            "#,
        )
        .bind(&step_run.id)
        .bind(&step_run.verification_run_id)
        .bind(&step_run.step_id)
        .bind(&step_run.step_name)
        .bind(step_run.status.to_string())
        .bind(step_run.required)
        .bind(step_run.exit_code)
        .bind(step_run.started_at_ms)
        .bind(step_run.finished_at_ms)
        .bind(step_run.duration_ms)
        .bind(step_run.stdout_preview.as_deref())
        .bind(step_run.stdout_truncated)
        .bind(step_run.stdout_bytes as i64)
        .bind(step_run.stdout_artifact_id.as_deref())
        .bind(step_run.stderr_preview.as_deref())
        .bind(step_run.stderr_truncated)
        .bind(step_run.stderr_bytes as i64)
        .bind(step_run.stderr_artifact_id.as_deref())
        .bind(serde_json::to_value(&step_run.artifacts)?)
        .bind(step_run.error_message.as_deref())
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Mark a verification run complete with its final outcome.
    pub async fn finalize_run(&self, run_id: &str, result: VerificationRunResult) -> Result<()> {
        let finished_at_ms = now_millis();
        let status_str = result.to_string();

        sqlx::query(
            r#"
            UPDATE orbit_verification_runs
            SET status = $1, overall_result = $2, finished_at_ms = $3
            WHERE id = $4
            "#,
        )
        .bind(&status_str)
        .bind(&status_str)
        .bind(finished_at_ms)
        .bind(run_id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Fetch a full verification run by ID with its steps.
    pub async fn get_run(&self, run_id: &str) -> Result<Option<VerificationRun>> {
        #[derive(FromRow)]
        struct RunRow {
            id: String,
            attempt_id: String,
            workspace_state_id: String,
            plan_id: String,
            plan_version: i32,
            plan_snapshot: serde_json::Value,
            status: String,
            environment_identity: serde_json::Value,
            started_at_ms: i64,
            finished_at_ms: Option<i64>,
            overall_result: Option<String>,
        }

        let run_opt = sqlx::query_as::<_, RunRow>(
            r#"
            SELECT id, attempt_id, workspace_state_id, plan_id, plan_version,
                   plan_snapshot, status, environment_identity, started_at_ms,
                   finished_at_ms, overall_result
            FROM orbit_verification_runs
            WHERE id = $1
            "#,
        )
        .bind(run_id)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = run_opt else {
            return Ok(None);
        };

        #[derive(FromRow)]
        struct StepRow {
            id: String,
            verification_run_id: String,
            step_id: String,
            step_name: String,
            status: String,
            required: bool,
            exit_code: Option<i32>,
            started_at_ms: i64,
            finished_at_ms: Option<i64>,
            duration_ms: Option<i64>,
            stdout_preview: Option<String>,
            stdout_truncated: bool,
            stdout_bytes: i64,
            stdout_artifact_id: Option<String>,
            stderr_preview: Option<String>,
            stderr_truncated: bool,
            stderr_bytes: i64,
            stderr_artifact_id: Option<String>,
            artifacts: serde_json::Value,
            error_message: Option<String>,
        }

        let step_rows = sqlx::query_as::<_, StepRow>(
            r#"
            SELECT id, verification_run_id, step_id, step_name, status, required,
                   exit_code, started_at_ms, finished_at_ms, duration_ms,
                   stdout_preview, stdout_truncated, stdout_bytes, stdout_artifact_id,
                   stderr_preview, stderr_truncated, stderr_bytes, stderr_artifact_id,
                   artifacts, error_message
            FROM orbit_verification_step_runs
            WHERE verification_run_id = $1
            ORDER BY created_at ASC
            "#,
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?;

        let step_runs = step_rows
            .into_iter()
            .map(|s| {
                let status = match s.status.as_str() {
                    "PENDING" => VerificationStepStatus::Pending,
                    "RUNNING" => VerificationStepStatus::Running,
                    "PASSED" => VerificationStepStatus::Passed,
                    "FAILED" => VerificationStepStatus::Failed,
                    "ERROR" => VerificationStepStatus::Error,
                    "TIMED_OUT" => VerificationStepStatus::TimedOut,
                    "CANCELLED" => VerificationStepStatus::Cancelled,
                    _ => VerificationStepStatus::Skipped,
                };
                let artifacts: Vec<crate::model::Artifact> =
                    serde_json::from_value(s.artifacts).unwrap_or_default();
                VerificationStepRun {
                    id: s.id,
                    verification_run_id: s.verification_run_id,
                    step_id: s.step_id,
                    step_name: s.step_name,
                    status,
                    required: s.required,
                    exit_code: s.exit_code,
                    started_at_ms: s.started_at_ms,
                    finished_at_ms: s.finished_at_ms,
                    duration_ms: s.duration_ms,
                    stdout_preview: s.stdout_preview,
                    stdout_truncated: s.stdout_truncated,
                    stdout_bytes: s.stdout_bytes.max(0) as u64,
                    stdout_artifact_id: s.stdout_artifact_id,
                    stderr_preview: s.stderr_preview,
                    stderr_truncated: s.stderr_truncated,
                    stderr_bytes: s.stderr_bytes.max(0) as u64,
                    stderr_artifact_id: s.stderr_artifact_id,
                    artifacts,
                    error_message: s.error_message,
                }
            })
            .collect();

        let overall_result = row.overall_result.as_deref().and_then(|res| match res {
            "PASSED" => Some(VerificationRunResult::Passed),
            "FAILED" => Some(VerificationRunResult::Failed),
            "ERROR" => Some(VerificationRunResult::Error),
            "TIMED_OUT" => Some(VerificationRunResult::TimedOut),
            "CANCELLED" => Some(VerificationRunResult::Cancelled),
            _ => None,
        });

        let status = match row.status.as_str() {
            "PENDING" => VerificationStepStatus::Pending,
            "RUNNING" => VerificationStepStatus::Running,
            "PASSED" => VerificationStepStatus::Passed,
            "FAILED" => VerificationStepStatus::Failed,
            "ERROR" => VerificationStepStatus::Error,
            "TIMED_OUT" => VerificationStepStatus::TimedOut,
            "CANCELLED" => VerificationStepStatus::Cancelled,
            _ => VerificationStepStatus::Skipped,
        };

        Ok(Some(VerificationRun {
            id: row.id,
            attempt_id: row.attempt_id,
            workspace_state_id: row.workspace_state_id,
            plan_id: row.plan_id,
            plan_version: row.plan_version as u32,
            plan_snapshot: serde_json::from_value(row.plan_snapshot)?,
            status,
            environment_identity: serde_json::from_value(row.environment_identity)?,
            started_at_ms: row.started_at_ms,
            finished_at_ms: row.finished_at_ms,
            overall_result,
            step_runs,
        }))
    }

    /// List all verification runs for a given attempt.
    pub async fn list_runs(&self, attempt_id: &str) -> Result<Vec<VerificationRun>> {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM orbit_verification_runs WHERE attempt_id = $1 ORDER BY created_at DESC",
        )
        .bind(attempt_id)
        .fetch_all(&self.pool)
        .await?;

        let mut runs = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(run) = self.get_run(&id).await? {
                runs.push(run);
            }
        }
        Ok(runs)
    }

    /// Check if a workspace state has a passing verification run.
    ///
    /// Critical invariant: evidence is valid ONLY for the exact `workspace_state_id`.
    /// If the workspace mutates to state B, queries for B will NOT match state A.
    pub async fn latest_passing_verification_for_workspace(
        &self,
        workspace_state_id: &str,
    ) -> Result<Option<VerificationRun>> {
        let run_id: Option<String> = sqlx::query_scalar(
            r#"
            SELECT id FROM orbit_verification_runs
            WHERE workspace_state_id = $1 AND overall_result = 'PASSED'
            ORDER BY created_at DESC
            LIMIT 1
            "#,
        )
        .bind(workspace_state_id)
        .fetch_optional(&self.pool)
        .await?;

        match run_id {
            Some(id) => self.get_run(&id).await,
            None => Ok(None),
        }
    }
}

/// Execute an entire VerificationPlan against a workspace directory, persisting evidence to PostgreSQL.
pub async fn execute_verification_plan(
    store: &VerificationStore,
    attempt_id: &str,
    workspace_state: &WorkspaceState,
    plan: &VerificationPlan,
    workspace_dir: &Path,
    environment: EnvironmentIdentity,
    cancellation_token: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<VerificationRun> {
    plan.validate()?;
    let run = store
        .create_run(attempt_id, workspace_state, plan, environment)
        .await?;

    let mut overall_result = VerificationRunResult::Passed;
    let mut step_runs = Vec::new();

    for step in &plan.steps {
        if cancellation_token
            .as_ref()
            .is_some_and(|token| *token.borrow())
        {
            overall_result = VerificationRunResult::Cancelled;
            break;
        }

        let step_run_id = format!("vstep-{}", crate::model::id());
        let step_started_at_ms = now_millis();

        let image_opt = run.environment_identity.runtime_image.as_deref();
        let capture_res = execute_verification_command_isolated(
            step,
            workspace_dir,
            None,
            cancellation_token.clone(),
            image_opt,
        )
        .await;

        let finished_at_ms = now_millis();
        let duration_ms = finished_at_ms - step_started_at_ms;

        let (
            status,
            exit_code,
            stdout_preview,
            stdout_bytes,
            stdout_trunc,
            stderr_preview,
            stderr_bytes,
            stderr_trunc,
            error_msg,
        ) = match capture_res {
            Ok(capture) => {
                let st = if capture.timed_out {
                    VerificationStepStatus::TimedOut
                } else if capture.exit_code == Some(0) {
                    VerificationStepStatus::Passed
                } else {
                    VerificationStepStatus::Failed
                };

                let out_preview = String::from_utf8_lossy(
                    &capture.stdout_bytes
                        [..capture.stdout_bytes.len().min(MAX_INLINE_OUTPUT_BYTES)],
                )
                .to_string();

                let err_preview = String::from_utf8_lossy(
                    &capture.stderr_bytes
                        [..capture.stderr_bytes.len().min(MAX_INLINE_OUTPUT_BYTES)],
                )
                .to_string();

                (
                    st,
                    capture.exit_code,
                    Some(out_preview),
                    capture.stdout_total_len,
                    capture.stdout_truncated
                        || capture.stdout_total_len > MAX_INLINE_OUTPUT_BYTES as u64,
                    Some(err_preview),
                    capture.stderr_total_len,
                    capture.stderr_truncated
                        || capture.stderr_total_len > MAX_INLINE_OUTPUT_BYTES as u64,
                    None,
                )
            }
            Err(err) => {
                let is_cancel = err.to_string().contains("cancelled");
                let st = if is_cancel {
                    VerificationStepStatus::Cancelled
                } else {
                    VerificationStepStatus::Error
                };
                (
                    st,
                    None,
                    None,
                    0,
                    false,
                    None,
                    0,
                    false,
                    Some(err.to_string()),
                )
            }
        };

        let step_record = VerificationStepRun {
            id: step_run_id,
            verification_run_id: run.id.clone(),
            step_id: step.id.clone(),
            step_name: step.name.clone(),
            status,
            required: step.required,
            exit_code,
            started_at_ms: step_started_at_ms,
            finished_at_ms: Some(finished_at_ms),
            duration_ms: Some(duration_ms),
            stdout_preview,
            stdout_truncated: stdout_trunc,
            stdout_bytes,
            stdout_artifact_id: None,
            stderr_preview,
            stderr_truncated: stderr_trunc,
            stderr_bytes,
            stderr_artifact_id: None,
            artifacts: Vec::new(),
            error_message: error_msg,
        };

        store.record_step_run(&step_record).await?;
        step_runs.push(step_record);

        if status != VerificationStepStatus::Passed && step.required {
            overall_result = match status {
                VerificationStepStatus::TimedOut => VerificationRunResult::TimedOut,
                VerificationStepStatus::Cancelled => VerificationRunResult::Cancelled,
                VerificationStepStatus::Error => VerificationRunResult::Error,
                _ => VerificationRunResult::Failed,
            };
            // Stop execution on first required failure (fail-fast per section 13)
            break;
        }
    }

    store.finalize_run(&run.id, overall_result).await?;
    let mut completed_run = store
        .get_run(&run.id)
        .await?
        .context("run record disappeared")?;
    completed_run.step_runs = step_runs;
    Ok(completed_run)
}

/// Format human-readable output for `orbit verification show <run_id>`.
pub fn format_verification_show(run: &VerificationRun) -> String {
    let mut out = String::new();
    out.push_str(&format!("Verification {}\n", run.id));
    out.push_str(&format!("Attempt       {}\n", run.attempt_id));
    out.push_str(&format!("Workspace     {}\n", run.workspace_state_id));
    let res_str = run
        .overall_result
        .map(|r| r.to_string())
        .unwrap_or_else(|| run.status.to_string());
    out.push_str(&format!("Result        {}\n", res_str));

    if let (Some(fin), start) = (run.finished_at_ms, run.started_at_ms) {
        let dur_sec = (fin - start) as f64 / 1000.0;
        out.push_str(&format!("Duration      {:.1}s\n", dur_sec));
    }

    out.push_str("\nSTEPS\n");
    out.push_str(&format!(
        "{:<16} {:<10} {:<10} {}\n",
        "NAME", "RESULT", "DURATION", "EXIT"
    ));

    for s in &run.step_runs {
        let dur_str = s
            .duration_ms
            .map(|d| format!("{:.1}s", d as f64 / 1000.0))
            .unwrap_or_else(|| "—".into());
        let exit_str = s
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "—".into());
        out.push_str(&format!(
            "{:<16} {:<10} {:<10} {}\n",
            s.step_name,
            s.status.to_string(),
            dur_str,
            exit_str
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workspace_state_computation_and_invalidation() {
        let ws_a1 =
            WorkspaceState::compute_from_parts("commit-0", "commit-0", Some("sha256-diff-A"));
        let ws_a2 =
            WorkspaceState::compute_from_parts("commit-0", "commit-0", Some("sha256-diff-A"));
        let ws_b =
            WorkspaceState::compute_from_parts("commit-0", "commit-0", Some("sha256-diff-B"));

        assert_eq!(ws_a1, ws_a2);
        assert_ne!(ws_a1.state_id, ws_b.state_id);
    }

    #[test]
    fn test_verification_plan_validation() {
        let mut plan = VerificationPlan::new(
            "rust-test",
            "Rust Standard Verification",
            vec![
                VerificationStep::new_command("fmt", "Format", vec!["cargo".into(), "fmt".into()]),
                VerificationStep::new_command("test", "Tests", vec!["cargo".into(), "test".into()]),
            ],
        );
        assert!(plan.validate().is_ok());

        // Duplicate step id fails
        plan.steps.push(VerificationStep::new_command(
            "fmt",
            "Format 2",
            vec!["cargo".into()],
        ));
        assert!(plan.validate().is_err());
    }

    #[tokio::test]
    async fn test_execute_verification_command_pass_and_fail() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let step_pass = VerificationStep::new_command(
            "echo_test",
            "Echo",
            vec!["echo".into(), "hello orbit".into()],
        );
        let cap_pass =
            execute_verification_command(&step_pass, temp_dir.path(), None, None).await?;
        assert_eq!(cap_pass.exit_code, Some(0));
        assert!(String::from_utf8_lossy(&cap_pass.stdout_bytes).contains("hello orbit"));
        assert!(!cap_pass.timed_out);

        let step_fail = VerificationStep::new_command(
            "fail_test",
            "Fail",
            vec!["sh".into(), "-c".into(), "exit 42".into()],
        );
        let cap_fail =
            execute_verification_command(&step_fail, temp_dir.path(), None, None).await?;
        assert_eq!(cap_fail.exit_code, Some(42));
        assert!(!cap_fail.timed_out);

        Ok(())
    }

    #[tokio::test]
    async fn test_execute_verification_command_timeout() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let mut step =
            VerificationStep::new_command("sleep_test", "Sleep", vec!["sleep".into(), "10".into()]);
        step.timeout_seconds = 1;
        let cap = execute_verification_command(
            &step,
            temp_dir.path(),
            Some(Duration::from_millis(100)),
            None,
        )
        .await?;
        assert!(cap.timed_out);
        assert!(cap.exit_code.is_none());

        Ok(())
    }

    #[tokio::test]
    async fn test_execute_verification_command_cancellation() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let step = VerificationStep::new_command(
            "long_sleep",
            "Long Sleep",
            vec!["sleep".into(), "30".into()],
        );
        let (tx, rx) = tokio::sync::watch::channel(false);

        let worker = tokio::spawn(async move {
            execute_verification_command(
                &step,
                temp_dir.path(),
                Some(Duration::from_secs(30)),
                Some(rx),
            )
            .await
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        tx.send(true)?;

        let res = worker.await?;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("cancelled"));

        Ok(())
    }

    #[tokio::test]
    async fn test_child_process_tree_cleanup() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let marker = temp_dir.path().join("child_alive.txt");
        let script = format!("(sleep 0.1 && echo alive > {}) & sleep 5", marker.display());
        let step = VerificationStep::new_command(
            "spawn_child",
            "Spawn Child",
            vec!["sh".into(), "-c".into(), script],
        );

        let cap = execute_verification_command(
            &step,
            temp_dir.path(),
            Some(Duration::from_millis(200)),
            None,
        )
        .await?;
        assert!(cap.timed_out);

        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(())
    }

    #[test]
    fn test_format_verification_show() {
        let run = VerificationRun {
            id: "vrun-1234".into(),
            attempt_id: "att-5678".into(),
            workspace_state_id: "ws-abcdef".into(),
            plan_id: "rust-standard".into(),
            plan_version: 1,
            plan_snapshot: VerificationPlan::new("rust-standard", "Rust", vec![]),
            status: VerificationStepStatus::Passed,
            environment_identity: Default::default(),
            started_at_ms: 1000,
            finished_at_ms: Some(43800),
            overall_result: Some(VerificationRunResult::Passed),
            step_runs: vec![
                VerificationStepRun {
                    id: "vstep-1".into(),
                    verification_run_id: "vrun-1234".into(),
                    step_id: "fmt".into(),
                    step_name: "fmt".into(),
                    status: VerificationStepStatus::Passed,
                    required: true,
                    exit_code: Some(0),
                    started_at_ms: 1000,
                    finished_at_ms: Some(1400),
                    duration_ms: Some(400),
                    stdout_preview: None,
                    stdout_truncated: false,
                    stdout_bytes: 0,
                    stdout_artifact_id: None,
                    stderr_preview: None,
                    stderr_truncated: false,
                    stderr_bytes: 0,
                    stderr_artifact_id: None,
                    artifacts: vec![],
                    error_message: None,
                },
                VerificationStepRun {
                    id: "vstep-2".into(),
                    verification_run_id: "vrun-1234".into(),
                    step_id: "tests".into(),
                    step_name: "tests".into(),
                    status: VerificationStepStatus::Passed,
                    required: true,
                    exit_code: Some(0),
                    started_at_ms: 1400,
                    finished_at_ms: Some(31700),
                    duration_ms: Some(30300),
                    stdout_preview: None,
                    stdout_truncated: false,
                    stdout_bytes: 0,
                    stdout_artifact_id: None,
                    stderr_preview: None,
                    stderr_truncated: false,
                    stderr_bytes: 0,
                    stderr_artifact_id: None,
                    artifacts: vec![],
                    error_message: None,
                },
            ],
        };

        let show = format_verification_show(&run);
        assert!(show.contains("Verification vrun-1234"));
        assert!(show.contains("Attempt       att-5678"));
        assert!(show.contains("Workspace     ws-abcdef"));
        assert!(show.contains("Result        PASSED"));
        assert!(show.contains("Duration      42.8s"));
        assert!(show.contains("fmt              PASSED     0.4s       0"));
        assert!(show.contains("tests            PASSED     30.3s      0"));
    }
}
