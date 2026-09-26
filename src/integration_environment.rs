//! Phase B4: Managed Integration Test Environments
//!
//! Extends Orbit's verification subsystem so it can test multi-process and multi-service applications:
//! - Orbit owns integration environment lifecycle (not agents).
//! - Clean isolated internal container networks per run.
//! - Managed container services (PostgreSQL, Redis, etc.) and managed workspace process services.
//! - Explicit readiness probes (TCP, HTTP, Process) with bounded timeouts.
//! - Dependency graph startup order and idempotent reverse-order cleanup.
//! - Setup / migration steps executed once dependency services are ready.
//! - Strict distinction between environment errors, readiness timeouts, and test assertion failures.
//! - Immutable environment digests and resolved image digests for durable qualification.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use std::{
    collections::{BTreeMap, HashSet},
    net::SocketAddr,
    path::Path,
    time::Duration,
};
use tokio::time::Instant;

use crate::verification::{
    MAX_INLINE_OUTPUT_BYTES, ScopedProcessGroup, VerificationNetworkPolicy, VerificationStep,
};

/// Service classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceKind {
    Container,
    Process,
}

impl std::fmt::Display for ServiceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Container => write!(f, "container"),
            Self::Process => write!(f, "process"),
        }
    }
}

/// Readiness probe specification.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReadinessProbe {
    Tcp {
        host: String,
        port: u16,
        #[serde(default = "default_probe_timeout_seconds")]
        timeout_seconds: u64,
        #[serde(default = "default_probe_interval_millis")]
        interval_ms: u64,
    },
    Http {
        url: String,
        #[serde(default = "default_expected_http_status")]
        expected_status: u16,
        #[serde(default = "default_probe_timeout_seconds")]
        timeout_seconds: u64,
        #[serde(default = "default_probe_interval_millis")]
        interval_ms: u64,
    },
    Process {
        #[serde(default = "default_probe_timeout_seconds")]
        timeout_seconds: u64,
        #[serde(default = "default_probe_interval_millis")]
        interval_ms: u64,
    },
}

fn default_probe_timeout_seconds() -> u64 {
    30
}

fn default_probe_interval_millis() -> u64 {
    250
}

fn default_expected_http_status() -> u16 {
    200
}

/// Specification for a single managed service in an integration environment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedServiceSpec {
    pub id: String,
    pub kind: ServiceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub internal_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness: Option<ReadinessProbe>,
    #[serde(default = "default_service_timeout")]
    pub timeout_seconds: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<String>,
}

fn default_service_timeout() -> u64 {
    60
}

impl ManagedServiceSpec {
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.id.trim().is_empty(), "service id required");
        match self.kind {
            ServiceKind::Container => {
                ensure!(
                    self.image.is_some(),
                    "container service '{}' requires an image",
                    self.id
                );
            }
            ServiceKind::Process => {
                ensure!(
                    !self.command.is_empty(),
                    "process service '{}' requires a command",
                    self.id
                );
            }
        }
        ensure!(self.timeout_seconds > 0, "timeout_seconds must be > 0");
        Ok(())
    }
}

/// Complete specification of an integration test environment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationEnvironmentSpec {
    pub id: String,
    #[serde(default = "default_version_one")]
    pub version: u32,
    #[serde(default)]
    pub services: Vec<ManagedServiceSpec>,
    #[serde(default)]
    pub setup_steps: Vec<VerificationStep>,
    #[serde(default)]
    pub network_policy: VerificationNetworkPolicy,
}

fn default_version_one() -> u32 {
    1
}

impl IntegrationEnvironmentSpec {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            version: 1,
            services: Vec::new(),
            setup_steps: Vec::new(),
            network_policy: VerificationNetworkPolicy::Isolated,
        }
    }

    pub fn digest(&self) -> String {
        let serialized = serde_json::to_string(self).unwrap_or_default();
        crate::model::digest(serialized.as_bytes())
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(!self.id.trim().is_empty(), "environment spec id required");
        ensure!(self.version > 0, "environment spec version must be > 0");

        let mut service_ids = HashSet::new();
        for s in &self.services {
            s.validate()?;
            ensure!(service_ids.insert(&s.id), "duplicate service id '{}'", s.id);
        }

        // Verify dependency references exist and detect cycles
        for s in &self.services {
            for dep in &s.dependencies {
                ensure!(
                    service_ids.contains(dep),
                    "service '{}' depends on unknown service '{}'",
                    s.id,
                    dep
                );
                ensure!(dep != &s.id, "service '{}' cannot depend on itself", s.id);
            }
        }

        // Topological ordering check to ensure no cycles
        let _ = self.startup_order()?;

        for step in &self.setup_steps {
            step.validate()?;
        }

        Ok(())
    }

    /// Compute dependency-ordered list of services for startup.
    pub fn startup_order(&self) -> Result<Vec<ManagedServiceSpec>> {
        let mut ordered = Vec::new();
        let mut visited = HashSet::new();
        let mut visiting = HashSet::new();

        let map: BTreeMap<String, &ManagedServiceSpec> =
            self.services.iter().map(|s| (s.id.clone(), s)).collect();

        fn dfs<'a>(
            id: &str,
            map: &BTreeMap<String, &'a ManagedServiceSpec>,
            visited: &mut HashSet<String>,
            visiting: &mut HashSet<String>,
            ordered: &mut Vec<&'a ManagedServiceSpec>,
        ) -> Result<()> {
            if visited.contains(id) {
                return Ok(());
            }
            if visiting.contains(id) {
                bail!("cyclic dependency detected involving service '{}'", id);
            }
            visiting.insert(id.to_string());

            if let Some(spec) = map.get(id) {
                for dep in &spec.dependencies {
                    dfs(dep, map, visited, visiting, ordered)?;
                }
                visiting.remove(id);
                visited.insert(id.to_string());
                ordered.push(spec);
            }
            Ok(())
        }

        for s in &self.services {
            if !visited.contains(&s.id) {
                dfs(&s.id, &map, &mut visited, &mut visiting, &mut ordered)?;
            }
        }

        Ok(ordered.into_iter().cloned().collect())
    }
}

/// Status of an environment run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EnvironmentRunStatus {
    Pending,
    Starting,
    Ready,
    Passed,
    Failed,
    Error,
    TimedOut,
    Cancelled,
}

impl std::fmt::Display for EnvironmentRunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "PENDING"),
            Self::Starting => write!(f, "STARTING"),
            Self::Ready => write!(f, "READY"),
            Self::Passed => write!(f, "PASSED"),
            Self::Failed => write!(f, "FAILED"),
            Self::Error => write!(f, "ERROR"),
            Self::TimedOut => write!(f, "TIMED_OUT"),
            Self::Cancelled => write!(f, "CANCELLED"),
        }
    }
}

/// Durable record of an EnvironmentRun.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnvironmentRun {
    pub id: String,
    pub verification_run_id: String,
    pub workspace_state_id: String,
    pub environment_spec_digest: String,
    pub status: EnvironmentRunStatus,
    pub network_name: Option<String>,
    pub network_policy: VerificationNetworkPolicy,
    pub started_at_ms: i64,
    pub ready_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub duration_ms: Option<i64>,
    pub error_message: Option<String>,
    pub environment_spec: IntegrationEnvironmentSpec,
    #[serde(default)]
    pub service_runs: Vec<EnvironmentServiceRun>,
}

/// Status of a managed service run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EnvironmentServiceStatus {
    Pending,
    Starting,
    Ready,
    Running,
    Passed,
    Failed,
    Error,
    TimedOut,
    Cancelled,
}

impl std::fmt::Display for EnvironmentServiceStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "PENDING"),
            Self::Starting => write!(f, "STARTING"),
            Self::Ready => write!(f, "READY"),
            Self::Running => write!(f, "RUNNING"),
            Self::Passed => write!(f, "PASSED"),
            Self::Failed => write!(f, "FAILED"),
            Self::Error => write!(f, "ERROR"),
            Self::TimedOut => write!(f, "TIMED_OUT"),
            Self::Cancelled => write!(f, "CANCELLED"),
        }
    }
}

/// Durable record of a single service run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnvironmentServiceRun {
    pub id: String,
    pub environment_run_id: String,
    pub service_id: String,
    pub service_kind: ServiceKind,
    pub status: EnvironmentServiceStatus,
    pub image_ref: Option<String>,
    pub resolved_image_digest: Option<String>,
    pub container_name: Option<String>,
    pub host_port: Option<u16>,
    pub internal_port: Option<u16>,
    pub started_at_ms: i64,
    pub ready_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub duration_ms: Option<i64>,
    pub stdout_preview: Option<String>,
    pub stdout_truncated: bool,
    pub stdout_bytes: u64,
    pub stderr_preview: Option<String>,
    pub stderr_truncated: bool,
    pub stderr_bytes: u64,
    pub error_message: Option<String>,
}

/// Store for integration environments.
#[derive(Clone)]
pub struct EnvironmentStore {
    pool: PgPool,
}

impl EnvironmentStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn create_environment_run(
        &self,
        verification_run_id: &str,
        workspace_state_id: &str,
        spec: &IntegrationEnvironmentSpec,
        network_name: Option<&str>,
    ) -> Result<EnvironmentRun> {
        spec.validate()?;
        let id = format!("envrun-{}", crate::model::id());
        let digest = spec.digest();
        let started_at_ms = crate::verification::now_millis();

        sqlx::query(
            r#"
            INSERT INTO orbit_environment_runs (
                id, verification_run_id, workspace_state_id, environment_spec_digest,
                status, network_name, network_policy, started_at_ms, environment_spec
            )
            VALUES ($1, $2, $3, $4, 'STARTING', $5, $6, $7, $8)
            "#,
        )
        .bind(&id)
        .bind(verification_run_id)
        .bind(workspace_state_id)
        .bind(&digest)
        .bind(network_name)
        .bind(spec.network_policy.to_string())
        .bind(started_at_ms)
        .bind(serde_json::to_value(spec)?)
        .execute(&self.pool)
        .await
        .context("insert orbit_environment_runs")?;

        Ok(EnvironmentRun {
            id,
            verification_run_id: verification_run_id.to_string(),
            workspace_state_id: workspace_state_id.to_string(),
            environment_spec_digest: digest,
            status: EnvironmentRunStatus::Starting,
            network_name: network_name.map(|s| s.to_string()),
            network_policy: spec.network_policy,
            started_at_ms,
            ready_at_ms: None,
            finished_at_ms: None,
            duration_ms: None,
            error_message: None,
            environment_spec: spec.clone(),
            service_runs: Vec::new(),
        })
    }

    pub async fn mark_environment_ready(&self, env_run_id: &str) -> Result<()> {
        let now = crate::verification::now_millis();
        sqlx::query(
            r#"
            UPDATE orbit_environment_runs
            SET status = 'READY', ready_at_ms = $1
            WHERE id = $2
            "#,
        )
        .bind(now)
        .bind(env_run_id)
        .execute(&self.pool)
        .await
        .context("mark environment ready")?;
        Ok(())
    }

    pub async fn finalize_environment_run(
        &self,
        env_run_id: &str,
        status: EnvironmentRunStatus,
        error_msg: Option<&str>,
    ) -> Result<()> {
        let now = crate::verification::now_millis();
        let status_str = status.to_string();

        sqlx::query(
            r#"
            UPDATE orbit_environment_runs
            SET status = $1, finished_at_ms = $2, duration_ms = $2 - started_at_ms, error_message = $3
            WHERE id = $4
            "#,
        )
        .bind(&status_str)
        .bind(now)
        .bind(error_msg)
        .bind(env_run_id)
        .execute(&self.pool)
        .await
        .context("finalize orbit_environment_runs")?;
        Ok(())
    }

    pub async fn record_service_run(&self, s_run: &EnvironmentServiceRun) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO orbit_environment_service_runs (
                id, environment_run_id, service_id, service_kind, status,
                image_ref, resolved_image_digest, container_name, host_port, internal_port,
                started_at_ms, ready_at_ms, finished_at_ms, duration_ms,
                stdout_preview, stdout_truncated, stdout_bytes,
                stderr_preview, stderr_truncated, stderr_bytes,
                error_message
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21)
            ON CONFLICT (id) DO UPDATE SET
                status = EXCLUDED.status,
                ready_at_ms = EXCLUDED.ready_at_ms,
                finished_at_ms = EXCLUDED.finished_at_ms,
                duration_ms = EXCLUDED.duration_ms,
                stdout_preview = EXCLUDED.stdout_preview,
                stdout_truncated = EXCLUDED.stdout_truncated,
                stdout_bytes = EXCLUDED.stdout_bytes,
                stderr_preview = EXCLUDED.stderr_preview,
                stderr_truncated = EXCLUDED.stderr_truncated,
                stderr_bytes = EXCLUDED.stderr_bytes,
                error_message = EXCLUDED.error_message
            "#,
        )
        .bind(&s_run.id)
        .bind(&s_run.environment_run_id)
        .bind(&s_run.service_id)
        .bind(s_run.service_kind.to_string())
        .bind(s_run.status.to_string())
        .bind(s_run.image_ref.as_deref())
        .bind(s_run.resolved_image_digest.as_deref())
        .bind(s_run.container_name.as_deref())
        .bind(s_run.host_port.map(|p| p as i32))
        .bind(s_run.internal_port.map(|p| p as i32))
        .bind(s_run.started_at_ms)
        .bind(s_run.ready_at_ms)
        .bind(s_run.finished_at_ms)
        .bind(s_run.duration_ms)
        .bind(s_run.stdout_preview.as_deref())
        .bind(s_run.stdout_truncated)
        .bind(s_run.stdout_bytes as i64)
        .bind(s_run.stderr_preview.as_deref())
        .bind(s_run.stderr_truncated)
        .bind(s_run.stderr_bytes as i64)
        .bind(s_run.error_message.as_deref())
        .execute(&self.pool)
        .await
        .context("record_service_run")?;
        Ok(())
    }

    pub async fn get_environment_run(&self, env_run_id: &str) -> Result<Option<EnvironmentRun>> {
        #[derive(FromRow)]
        struct EnvRow {
            id: String,
            verification_run_id: String,
            workspace_state_id: String,
            environment_spec_digest: String,
            status: String,
            network_name: Option<String>,
            network_policy: String,
            started_at_ms: i64,
            ready_at_ms: Option<i64>,
            finished_at_ms: Option<i64>,
            duration_ms: Option<i64>,
            error_message: Option<String>,
            environment_spec: serde_json::Value,
        }

        let row = sqlx::query_as::<_, EnvRow>(
            r#"
            SELECT id, verification_run_id, workspace_state_id, environment_spec_digest,
                   status, network_name, network_policy, started_at_ms, ready_at_ms,
                   finished_at_ms, duration_ms, error_message, environment_spec
            FROM orbit_environment_runs
            WHERE id = $1
            "#,
        )
        .bind(env_run_id)
        .fetch_optional(&self.pool)
        .await
        .context("get_environment_run")?;

        let Some(r) = row else { return Ok(None) };

        #[derive(FromRow)]
        struct SRow {
            id: String,
            environment_run_id: String,
            service_id: String,
            service_kind: String,
            status: String,
            image_ref: Option<String>,
            resolved_image_digest: Option<String>,
            container_name: Option<String>,
            host_port: Option<i32>,
            internal_port: Option<i32>,
            started_at_ms: i64,
            ready_at_ms: Option<i64>,
            finished_at_ms: Option<i64>,
            duration_ms: Option<i64>,
            stdout_preview: Option<String>,
            stdout_truncated: bool,
            stdout_bytes: i64,
            stderr_preview: Option<String>,
            stderr_truncated: bool,
            stderr_bytes: i64,
            error_message: Option<String>,
        }

        let srows = sqlx::query_as::<_, SRow>(
            r#"
            SELECT id, environment_run_id, service_id, service_kind, status,
                   image_ref, resolved_image_digest, container_name, host_port, internal_port,
                   started_at_ms, ready_at_ms, finished_at_ms, duration_ms,
                   stdout_preview, stdout_truncated, stdout_bytes,
                   stderr_preview, stderr_truncated, stderr_bytes, error_message
            FROM orbit_environment_service_runs
            WHERE environment_run_id = $1
            ORDER BY created_at ASC
            "#,
        )
        .bind(env_run_id)
        .fetch_all(&self.pool)
        .await
        .context("fetch service runs")?;

        let service_runs = srows
            .into_iter()
            .map(|s| {
                let status = match s.status.as_str() {
                    "PENDING" => EnvironmentServiceStatus::Pending,
                    "STARTING" => EnvironmentServiceStatus::Starting,
                    "READY" => EnvironmentServiceStatus::Ready,
                    "RUNNING" => EnvironmentServiceStatus::Running,
                    "PASSED" => EnvironmentServiceStatus::Passed,
                    "FAILED" => EnvironmentServiceStatus::Failed,
                    "ERROR" => EnvironmentServiceStatus::Error,
                    "TIMED_OUT" => EnvironmentServiceStatus::TimedOut,
                    "CANCELLED" => EnvironmentServiceStatus::Cancelled,
                    _ => EnvironmentServiceStatus::Error,
                };
                let service_kind = if s.service_kind == "process" {
                    ServiceKind::Process
                } else {
                    ServiceKind::Container
                };
                EnvironmentServiceRun {
                    id: s.id,
                    environment_run_id: s.environment_run_id,
                    service_id: s.service_id,
                    service_kind,
                    status,
                    image_ref: s.image_ref,
                    resolved_image_digest: s.resolved_image_digest,
                    container_name: s.container_name,
                    host_port: s.host_port.map(|p| p as u16),
                    internal_port: s.internal_port.map(|p| p as u16),
                    started_at_ms: s.started_at_ms,
                    ready_at_ms: s.ready_at_ms,
                    finished_at_ms: s.finished_at_ms,
                    duration_ms: s.duration_ms,
                    stdout_preview: s.stdout_preview,
                    stdout_truncated: s.stdout_truncated,
                    stdout_bytes: s.stdout_bytes.max(0) as u64,
                    stderr_preview: s.stderr_preview,
                    stderr_truncated: s.stderr_truncated,
                    stderr_bytes: s.stderr_bytes.max(0) as u64,
                    error_message: s.error_message,
                }
            })
            .collect();

        let status = match r.status.as_str() {
            "PENDING" => EnvironmentRunStatus::Pending,
            "STARTING" => EnvironmentRunStatus::Starting,
            "READY" => EnvironmentRunStatus::Ready,
            "PASSED" => EnvironmentRunStatus::Passed,
            "FAILED" => EnvironmentRunStatus::Failed,
            "ERROR" => EnvironmentRunStatus::Error,
            "TIMED_OUT" => EnvironmentRunStatus::TimedOut,
            "CANCELLED" => EnvironmentRunStatus::Cancelled,
            _ => EnvironmentRunStatus::Error,
        };

        let net_pol = if r.network_policy == "none" {
            VerificationNetworkPolicy::None
        } else {
            VerificationNetworkPolicy::Isolated
        };

        Ok(Some(EnvironmentRun {
            id: r.id,
            verification_run_id: r.verification_run_id,
            workspace_state_id: r.workspace_state_id,
            environment_spec_digest: r.environment_spec_digest,
            status,
            network_name: r.network_name,
            network_policy: net_pol,
            started_at_ms: r.started_at_ms,
            ready_at_ms: r.ready_at_ms,
            finished_at_ms: r.finished_at_ms,
            duration_ms: r.duration_ms,
            error_message: r.error_message,
            environment_spec: serde_json::from_value(r.environment_spec)?,
            service_runs,
        }))
    }

    pub async fn get_environment_run_for_verification(
        &self,
        vrun_id: &str,
    ) -> Result<Option<EnvironmentRun>> {
        let env_id = sqlx::query_scalar::<_, String>(
            "SELECT id FROM orbit_environment_runs WHERE verification_run_id = $1 ORDER BY created_at DESC LIMIT 1",
        )
        .bind(vrun_id)
        .fetch_optional(&self.pool)
        .await
        .context("get_environment_run_for_verification")?;

        match env_id {
            Some(id) => self.get_environment_run(&id).await,
            None => Ok(None),
        }
    }
}

/// Handle to an active running service for live log collection and process lifecycle.
pub struct ActiveServiceHandle {
    pub spec: ManagedServiceSpec,
    pub run_record: EnvironmentServiceRun,
    pub container_name: Option<String>,
    pub child: Option<tokio::process::Child>,
    pub pid_guard: Option<ScopedProcessGroup>,
    pub stdout_task: Option<tokio::task::JoinHandle<(Vec<u8>, u64, bool)>>,
    pub stderr_task: Option<tokio::task::JoinHandle<(Vec<u8>, u64, bool)>>,
}

/// Controller managing the full integration environment execution lifecycle.
pub struct EnvironmentManager {
    store: EnvironmentStore,
}

impl EnvironmentManager {
    pub fn new(store: EnvironmentStore) -> Self {
        Self { store }
    }

    /// Resolve an image tag/reference to its immutable content digest.
    pub async fn resolve_image_digest(image_ref: &str) -> Option<String> {
        let mut cmd = tokio::process::Command::new("podman");
        cmd.args([
            "--remote=false",
            "--cgroup-manager=cgroupfs",
            "image",
            "inspect",
            image_ref,
            "--format",
            "{{if .Digest}}{{.Digest}}{{else}}{{.Id}}{{end}}",
        ]);
        let out = cmd.output().await.ok()?;
        if out.status.success() {
            let digest_str = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !digest_str.is_empty() {
                return Some(digest_str);
            }
        }
        None
    }

    /// Execute readiness probe against a service.
    pub async fn poll_readiness(
        probe: &ReadinessProbe,
        network_name: Option<&str>,
        mut active_handle: Option<&mut ActiveServiceHandle>,
    ) -> Result<()> {
        match probe {
            ReadinessProbe::Tcp {
                host,
                port,
                timeout_seconds,
                interval_ms,
            } => {
                let deadline = Instant::now() + Duration::from_secs(*timeout_seconds);
                let poll_interval = Duration::from_millis(*interval_ms);

                let target_host = if let Some(ref handle) = active_handle {
                    if (host == "localhost" || host == "127.0.0.1") && network_name.is_some() {
                        handle.spec.id.clone()
                    } else {
                        host.clone()
                    }
                } else {
                    host.clone()
                };

                loop {
                    // Check if process child exited early
                    if let Some(ref mut handle) = active_handle
                        && let Some(ref mut child) = handle.child
                        && let Ok(Some(status)) = child.try_wait()
                    {
                        bail!(
                            "service '{}' exited prematurely before readiness with exit code: {:?}",
                            handle.spec.id,
                            status.code()
                        );
                    }

                    // Check if container exited early
                    if let Some(ref handle) = active_handle
                        && let Some(ref cname) = handle.container_name
                    {
                        let mut inspect_cmd = tokio::process::Command::new("podman");
                        inspect_cmd.args([
                            "--remote=false",
                            "--cgroup-manager=cgroupfs",
                            "inspect",
                            "-f",
                            "{{.State.Status}} {{.State.ExitCode}}",
                            cname,
                        ]);
                        if let Ok(out) = inspect_cmd.output().await
                            && out.status.success()
                        {
                            let st_str = String::from_utf8_lossy(&out.stdout);
                            let parts: Vec<&str> = st_str.split_whitespace().collect();
                            if let Some(state) = parts.first()
                                && (*state == "exited" || *state == "dead")
                            {
                                let code = parts.get(1).unwrap_or(&"unknown");
                                bail!(
                                    "service '{}' exited prematurely before readiness with exit code: {}",
                                    handle.spec.id,
                                    code
                                );
                            }
                        }
                    }

                    if Instant::now() > deadline {
                        bail!(
                            "readiness timeout: TCP probe to {}:{} did not succeed within {}s",
                            host,
                            port,
                            timeout_seconds
                        );
                    }

                    // Attempt connection
                    // If network_name is present and host is container name, run small probe inside container network
                    if let Some(net) = network_name {
                        let mut probe_cmd = tokio::process::Command::new("podman");
                        probe_cmd.args([
                            "--remote=false",
                            "--cgroup-manager=cgroupfs",
                            "run",
                            "--rm",
                            "--network",
                            net,
                            "docker.io/library/alpine:latest",
                            "nc",
                            "-z",
                            "-w",
                            "1",
                            &target_host,
                            &port.to_string(),
                        ]);
                        if let Ok(out) = probe_cmd.output().await
                            && out.status.success()
                        {
                            return Ok(());
                        }
                    } else {
                        let addr_str = format!("{}:{}", host, port);
                        if let Ok(socket_addr) = addr_str.parse::<SocketAddr>() {
                            if tokio::net::TcpStream::connect(socket_addr).await.is_ok() {
                                return Ok(());
                            }
                        } else if tokio::net::TcpStream::connect((host.as_str(), *port))
                            .await
                            .is_ok()
                        {
                            return Ok(());
                        }
                    }

                    tokio::time::sleep(poll_interval).await;
                }
            }
            ReadinessProbe::Http {
                url,
                expected_status,
                timeout_seconds,
                interval_ms,
            } => {
                let deadline = Instant::now() + Duration::from_secs(*timeout_seconds);
                let poll_interval = Duration::from_millis(*interval_ms);

                let target_url = if let Some(ref handle) = active_handle {
                    if network_name.is_some() {
                        url.replace("localhost", &handle.spec.id)
                            .replace("127.0.0.1", &handle.spec.id)
                    } else {
                        url.clone()
                    }
                } else {
                    url.clone()
                };

                loop {
                    if let Some(ref mut handle) = active_handle
                        && let Some(ref mut child) = handle.child
                        && let Ok(Some(status)) = child.try_wait()
                    {
                        bail!(
                            "service '{}' exited prematurely before readiness with exit code: {:?}",
                            handle.spec.id,
                            status.code()
                        );
                    }

                    // Check if container exited early
                    if let Some(ref handle) = active_handle
                        && let Some(ref cname) = handle.container_name
                    {
                        let mut inspect_cmd = tokio::process::Command::new("podman");
                        inspect_cmd.args([
                            "--remote=false",
                            "--cgroup-manager=cgroupfs",
                            "inspect",
                            "-f",
                            "{{.State.Status}} {{.State.ExitCode}}",
                            cname,
                        ]);
                        if let Ok(out) = inspect_cmd.output().await
                            && out.status.success()
                        {
                            let st_str = String::from_utf8_lossy(&out.stdout);
                            let parts: Vec<&str> = st_str.split_whitespace().collect();
                            if let Some(state) = parts.first()
                                && (*state == "exited" || *state == "dead")
                            {
                                let code = parts.get(1).unwrap_or(&"unknown");
                                bail!(
                                    "service '{}' exited prematurely before readiness with exit code: {}",
                                    handle.spec.id,
                                    code
                                );
                            }
                        }
                    }

                    if Instant::now() > deadline {
                        bail!(
                            "readiness timeout: HTTP probe to {} did not succeed within {}s",
                            url,
                            timeout_seconds
                        );
                    }

                    if let Some(net) = network_name {
                        let mut probe_cmd = tokio::process::Command::new("podman");
                        probe_cmd.args([
                            "--remote=false",
                            "--cgroup-manager=cgroupfs",
                            "run",
                            "--rm",
                            "--network",
                            net,
                            "docker.io/library/alpine:latest",
                            "wget",
                            "-q",
                            "-O",
                            "-",
                            &target_url,
                        ]);
                        if let Ok(out) = probe_cmd.output().await
                            && out.status.success()
                        {
                            return Ok(());
                        }
                    } else if let Ok(resp) = reqwest::get(url).await
                        && resp.status().as_u16() == *expected_status
                    {
                        return Ok(());
                    }

                    tokio::time::sleep(poll_interval).await;
                }
            }
            ReadinessProbe::Process {
                timeout_seconds: _,
                interval_ms,
            } => {
                let poll_interval = Duration::from_millis(*interval_ms);
                tokio::time::sleep(poll_interval).await;
                if let Some(ref mut handle) = active_handle {
                    if let Some(ref cname) = handle.container_name {
                        let mut inspect_cmd = tokio::process::Command::new("podman");
                        inspect_cmd.args([
                            "--remote=false",
                            "--cgroup-manager=cgroupfs",
                            "inspect",
                            "-f",
                            "{{.State.Status}} {{.State.ExitCode}}",
                            cname,
                        ]);
                        match inspect_cmd.output().await {
                            Ok(out) if out.status.success() => {
                                let st_str = String::from_utf8_lossy(&out.stdout);
                                let parts: Vec<&str> = st_str.split_whitespace().collect();
                                if let Some(state) = parts.first()
                                    && (*state == "exited" || *state == "dead")
                                {
                                    let code = parts.get(1).unwrap_or(&"unknown");
                                    bail!(
                                        "process service '{}' exited prematurely before readiness with exit code: {}",
                                        handle.spec.id,
                                        code
                                    );
                                }
                                Ok(())
                            }
                            Ok(out) => {
                                bail!(
                                    "failed to inspect process service container '{}': {}",
                                    cname,
                                    String::from_utf8_lossy(&out.stderr)
                                );
                            }
                            Err(e) => {
                                bail!("failed to run podman inspect on '{}': {e}", cname);
                            }
                        }
                    } else if let Some(ref mut child) = handle.child {
                        match child.try_wait() {
                            Ok(Some(status)) => {
                                bail!(
                                    "process service '{}' terminated unexpectedly with code: {:?}",
                                    handle.spec.id,
                                    status.code()
                                );
                            }
                            Ok(None) => Ok(()),
                            Err(e) => bail!("failed to check child process state: {e}"),
                        }
                    } else {
                        Ok(())
                    }
                } else {
                    Ok(())
                }
            }
        }
    }

    /// Spin up and coordinate an entire integration environment run.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_environment_lifecycle<F, Fut, T>(
        &self,
        verification_run_id: &str,
        workspace_state_id: &str,
        spec: &IntegrationEnvironmentSpec,
        workspace_dir: &Path,
        default_runtime_image: Option<&str>,
        cancellation_token: Option<tokio::sync::watch::Receiver<bool>>,
        test_execution_fn: F,
    ) -> Result<(EnvironmentRun, Result<T, (EnvironmentRunStatus, String)>)>
    where
        F: FnOnce(Option<String>) -> Fut,
        Fut: std::future::Future<Output = Result<T, (EnvironmentRunStatus, String)>>,
    {
        spec.validate()?;
        let canonical_workspace = workspace_dir.canonicalize()?;

        // 1. Create isolated podman network if required
        let network_name = if spec.network_policy == VerificationNetworkPolicy::Isolated {
            let net = format!("orbit-net-{}", crate::model::id());
            let mut create_net = tokio::process::Command::new("podman");
            create_net.args([
                "--remote=false",
                "--cgroup-manager=cgroupfs",
                "network",
                "create",
                "--internal",
                &net,
            ]);
            let res = create_net.output().await?;
            if !res.status.success() {
                bail!(
                    "failed to create isolated podman network '{}': {}",
                    net,
                    String::from_utf8_lossy(&res.stderr)
                );
            }
            Some(net)
        } else {
            None
        };

        // 2. Initialize environment run record in database
        let env_run = self
            .store
            .create_environment_run(
                verification_run_id,
                workspace_state_id,
                spec,
                network_name.as_deref(),
            )
            .await?;

        let ordered_services = spec.startup_order()?;
        let mut active_services: Vec<ActiveServiceHandle> = Vec::new();
        let mut environment_err: Option<(EnvironmentRunStatus, String)> = None;

        // 3. Start services in dependency order
        for svc in ordered_services {
            if cancellation_token
                .as_ref()
                .is_some_and(|token| *token.borrow())
            {
                environment_err = Some((
                    EnvironmentRunStatus::Cancelled,
                    "Environment startup cancelled".into(),
                ));
                break;
            }

            let s_run_id = format!("envs-{}-{}", svc.id, crate::model::id());
            let started_at_ms = crate::verification::now_millis();

            match svc.kind {
                ServiceKind::Container => {
                    let image_ref = svc.image.clone().unwrap_or_default();
                    let resolved_digest = Self::resolve_image_digest(&image_ref).await;
                    let cname = format!("orbit-svc-{}-{}", svc.id, crate::model::id());

                    let mut c = tokio::process::Command::new("podman");
                    c.args([
                        "--remote=false",
                        "--cgroup-manager=cgroupfs",
                        "run",
                        "-d",
                        "--name",
                        &cname,
                        "--label",
                        &format!("orbit.environment_run_id={}", env_run.id),
                        "--label",
                        &format!("orbit.service_id={}", svc.id),
                        "--label",
                        "orbit.service_kind=container",
                        "--security-opt=no-new-privileges",
                        "--pids-limit=256",
                        "--init",
                    ]);

                    if let Some(ref net) = network_name {
                        c.args(["--network", net, "--network-alias", &svc.id]);
                    } else {
                        c.arg("--network=none");
                    }

                    for (k, v) in &svc.env {
                        c.arg("--env").arg(format!("{}={}", k, v));
                    }

                    for m in &svc.mounts {
                        c.arg("--mount").arg(m);
                    }

                    c.arg(&image_ref);
                    if !svc.command.is_empty() {
                        c.args(&svc.command);
                    }
                    if !svc.args.is_empty() {
                        c.args(&svc.args);
                    }

                    let spawn_res = c.output().await;
                    match spawn_res {
                        Ok(out) if out.status.success() => {
                            let mut handle = ActiveServiceHandle {
                                spec: svc.clone(),
                                run_record: EnvironmentServiceRun {
                                    id: s_run_id.clone(),
                                    environment_run_id: env_run.id.clone(),
                                    service_id: svc.id.clone(),
                                    service_kind: svc.kind,
                                    status: EnvironmentServiceStatus::Starting,
                                    image_ref: Some(image_ref),
                                    resolved_image_digest: resolved_digest,
                                    container_name: Some(cname.clone()),
                                    host_port: None,
                                    internal_port: svc.internal_port,
                                    started_at_ms,
                                    ready_at_ms: None,
                                    finished_at_ms: None,
                                    duration_ms: None,
                                    stdout_preview: None,
                                    stdout_truncated: false,
                                    stdout_bytes: 0,
                                    stderr_preview: None,
                                    stderr_truncated: false,
                                    stderr_bytes: 0,
                                    error_message: None,
                                },
                                container_name: Some(cname.clone()),
                                child: None,
                                pid_guard: None,
                                stdout_task: None,
                                stderr_task: None,
                            };

                            // Poll readiness
                            if let Some(ref probe) = svc.readiness
                                && let Err(e) = Self::poll_readiness(
                                    probe,
                                    network_name.as_deref(),
                                    Some(&mut handle),
                                )
                                .await
                            {
                                let err_str = e.to_string();
                                let is_timeout = err_str.contains("timeout");
                                let status = if is_timeout {
                                    EnvironmentServiceStatus::TimedOut
                                } else {
                                    EnvironmentServiceStatus::Error
                                };
                                handle.run_record.status = status;
                                handle.run_record.error_message = Some(err_str.clone());

                                // Collect container logs
                                let mut logs_cmd = tokio::process::Command::new("podman");
                                logs_cmd.args([
                                    "--remote=false",
                                    "--cgroup-manager=cgroupfs",
                                    "logs",
                                    &cname,
                                ]);
                                if let Ok(lout) = logs_cmd.output().await {
                                    let stdout_str = String::from_utf8_lossy(&lout.stdout);
                                    let stderr_str = String::from_utf8_lossy(&lout.stderr);
                                    let stdout_bytes = lout.stdout.len() as u64;
                                    let stderr_bytes = lout.stderr.len() as u64;

                                    handle.run_record.stdout_bytes = stdout_bytes;
                                    handle.run_record.stdout_truncated =
                                        stdout_bytes > MAX_INLINE_OUTPUT_BYTES as u64;
                                    handle.run_record.stdout_preview =
                                        Some(if stdout_str.len() > MAX_INLINE_OUTPUT_BYTES {
                                            stdout_str[..MAX_INLINE_OUTPUT_BYTES].to_string()
                                        } else {
                                            stdout_str.to_string()
                                        });

                                    handle.run_record.stderr_bytes = stderr_bytes;
                                    handle.run_record.stderr_truncated =
                                        stderr_bytes > MAX_INLINE_OUTPUT_BYTES as u64;
                                    handle.run_record.stderr_preview =
                                        Some(if stderr_str.len() > MAX_INLINE_OUTPUT_BYTES {
                                            stderr_str[..MAX_INLINE_OUTPUT_BYTES].to_string()
                                        } else {
                                            stderr_str.to_string()
                                        });
                                }

                                self.store.record_service_run(&handle.run_record).await.ok();
                                active_services.push(handle);
                                let env_st = if is_timeout {
                                    EnvironmentRunStatus::TimedOut
                                } else {
                                    EnvironmentRunStatus::Error
                                };
                                environment_err = Some((env_st, err_str));
                                break;
                            }

                            let ready_at_ms = crate::verification::now_millis();
                            handle.run_record.ready_at_ms = Some(ready_at_ms);
                            handle.run_record.status = EnvironmentServiceStatus::Ready;
                            self.store.record_service_run(&handle.run_record).await.ok();
                            active_services.push(handle);
                        }
                        Ok(out) => {
                            let err_msg = format!(
                                "failed to start container service '{}': {}",
                                svc.id,
                                String::from_utf8_lossy(&out.stderr)
                            );
                            let srec = EnvironmentServiceRun {
                                id: s_run_id,
                                environment_run_id: env_run.id.clone(),
                                service_id: svc.id.clone(),
                                service_kind: svc.kind,
                                status: EnvironmentServiceStatus::Error,
                                image_ref: Some(image_ref),
                                resolved_image_digest: resolved_digest,
                                container_name: Some(cname),
                                host_port: None,
                                internal_port: svc.internal_port,
                                started_at_ms,
                                ready_at_ms: None,
                                finished_at_ms: Some(crate::verification::now_millis()),
                                duration_ms: Some(0),
                                stdout_preview: None,
                                stdout_truncated: false,
                                stdout_bytes: 0,
                                stderr_preview: Some(String::from_utf8_lossy(&out.stderr).into()),
                                stderr_truncated: false,
                                stderr_bytes: out.stderr.len() as u64,
                                error_message: Some(err_msg.clone()),
                            };
                            self.store.record_service_run(&srec).await.ok();
                            environment_err = Some((EnvironmentRunStatus::Error, err_msg));
                            break;
                        }
                        Err(e) => {
                            let err_msg = format!("failed to spawn podman: {e}");
                            let srec = EnvironmentServiceRun {
                                id: s_run_id,
                                environment_run_id: env_run.id.clone(),
                                service_id: svc.id.clone(),
                                service_kind: svc.kind,
                                status: EnvironmentServiceStatus::Error,
                                image_ref: Some(image_ref),
                                resolved_image_digest: resolved_digest,
                                container_name: Some(cname),
                                host_port: None,
                                internal_port: svc.internal_port,
                                started_at_ms,
                                ready_at_ms: None,
                                finished_at_ms: Some(crate::verification::now_millis()),
                                duration_ms: Some(0),
                                stdout_preview: None,
                                stdout_truncated: false,
                                stdout_bytes: 0,
                                stderr_preview: None,
                                stderr_truncated: false,
                                stderr_bytes: 0,
                                error_message: Some(err_msg.clone()),
                            };
                            self.store.record_service_run(&srec).await.ok();
                            environment_err = Some((EnvironmentRunStatus::Error, err_msg));
                            break;
                        }
                    }
                }
                ServiceKind::Process => {
                    let fallback_image =
                        default_runtime_image.unwrap_or("docker.io/library/alpine:latest");
                    let image_ref = svc.image.as_deref().unwrap_or(fallback_image);
                    let resolved_digest = Self::resolve_image_digest(image_ref).await;
                    let cname = format!("orbit-proc-{}-{}", svc.id, crate::model::id());

                    let mut c = tokio::process::Command::new("podman");
                    c.args([
                        "--remote=false",
                        "--cgroup-manager=cgroupfs",
                        "run",
                        "-d",
                        "--name",
                        &cname,
                        "--label",
                        &format!("orbit.environment_run_id={}", env_run.id),
                        "--label",
                        &format!("orbit.service_id={}", svc.id),
                        "--label",
                        "orbit.service_kind=process",
                        "--security-opt=no-new-privileges",
                        "--cap-drop=ALL",
                        "--cap-add=NET_BIND_SERVICE",
                        "--pids-limit=256",
                        "--init",
                        "--tmpfs",
                        "/tmp:rw,nosuid,nodev,size=67108864",
                        "--mount",
                        &format!(
                            "type=bind,src={},dst=/workspace",
                            canonical_workspace.display()
                        ),
                        "--workdir",
                        "/workspace",
                    ]);

                    if let Some(ref net) = network_name {
                        c.args(["--network", net, "--network-alias", &svc.id]);
                    } else {
                        c.arg("--network=none");
                    }

                    // Strict isolated deterministic environment allowlist
                    let mut env_map = BTreeMap::new();
                    env_map.insert("HOME".to_string(), "/tmp/orbit-home".to_string());
                    env_map.insert("CI".to_string(), "1".to_string());
                    env_map.insert("LANG".to_string(), "C.UTF-8".to_string());
                    env_map.insert("LC_ALL".to_string(), "C.UTF-8".to_string());
                    env_map.insert("TERM".to_string(), "dumb".to_string());
                    env_map.insert(
                        "PATH".to_string(),
                        "/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
                    );
                    env_map.insert("ORBIT_PROCESS_SERVICE".to_string(), "1".to_string());
                    env_map.insert("ORBIT_ENVIRONMENT_RUN_ID".to_string(), env_run.id.clone());
                    env_map.insert("ORBIT_SERVICE_ID".to_string(), svc.id.clone());

                    for (k, v) in &svc.env {
                        env_map.insert(k.clone(), v.clone());
                    }

                    for (k, v) in &env_map {
                        c.arg("--env").arg(format!("{}={}", k, v));
                    }

                    for m in &svc.mounts {
                        c.arg("--mount").arg(m);
                    }

                    c.arg(image_ref);

                    let canonical_str = canonical_workspace.to_string_lossy();
                    for arg in &svc.command {
                        if arg.starts_with(canonical_str.as_ref()) {
                            let suffix = &arg[canonical_str.len()..];
                            c.arg(format!("/workspace{}", suffix));
                        } else {
                            c.arg(arg);
                        }
                    }
                    for arg in &svc.args {
                        if arg.starts_with(canonical_str.as_ref()) {
                            let suffix = &arg[canonical_str.len()..];
                            c.arg(format!("/workspace{}", suffix));
                        } else {
                            c.arg(arg);
                        }
                    }

                    let spawn_res = c.output().await;
                    match spawn_res {
                        Ok(out) if out.status.success() => {
                            let mut handle = ActiveServiceHandle {
                                spec: svc.clone(),
                                run_record: EnvironmentServiceRun {
                                    id: s_run_id.clone(),
                                    environment_run_id: env_run.id.clone(),
                                    service_id: svc.id.clone(),
                                    service_kind: svc.kind,
                                    status: EnvironmentServiceStatus::Starting,
                                    image_ref: Some(image_ref.to_string()),
                                    resolved_image_digest: resolved_digest,
                                    container_name: Some(cname.clone()),
                                    host_port: None,
                                    internal_port: svc.internal_port,
                                    started_at_ms,
                                    ready_at_ms: None,
                                    finished_at_ms: None,
                                    duration_ms: None,
                                    stdout_preview: None,
                                    stdout_truncated: false,
                                    stdout_bytes: 0,
                                    stderr_preview: None,
                                    stderr_truncated: false,
                                    stderr_bytes: 0,
                                    error_message: None,
                                },
                                container_name: Some(cname.clone()),
                                child: None,
                                pid_guard: None,
                                stdout_task: None,
                                stderr_task: None,
                            };

                            // Poll readiness
                            if let Some(ref probe) = svc.readiness
                                && let Err(e) = Self::poll_readiness(
                                    probe,
                                    network_name.as_deref(),
                                    Some(&mut handle),
                                )
                                .await
                            {
                                let err_str = e.to_string();
                                let is_timeout = err_str.contains("timeout");
                                let status = if is_timeout {
                                    EnvironmentServiceStatus::TimedOut
                                } else {
                                    EnvironmentServiceStatus::Error
                                };
                                handle.run_record.status = status;
                                handle.run_record.error_message = Some(err_str.clone());

                                // Collect container logs
                                let mut logs_cmd = tokio::process::Command::new("podman");
                                logs_cmd.args([
                                    "--remote=false",
                                    "--cgroup-manager=cgroupfs",
                                    "logs",
                                    &cname,
                                ]);
                                if let Ok(lout) = logs_cmd.output().await {
                                    let stdout_str = String::from_utf8_lossy(&lout.stdout);
                                    let stderr_str = String::from_utf8_lossy(&lout.stderr);
                                    let stdout_bytes = lout.stdout.len() as u64;
                                    let stderr_bytes = lout.stderr.len() as u64;

                                    handle.run_record.stdout_bytes = stdout_bytes;
                                    handle.run_record.stdout_truncated =
                                        stdout_bytes > MAX_INLINE_OUTPUT_BYTES as u64;
                                    handle.run_record.stdout_preview =
                                        Some(if stdout_str.len() > MAX_INLINE_OUTPUT_BYTES {
                                            stdout_str[..MAX_INLINE_OUTPUT_BYTES].to_string()
                                        } else {
                                            stdout_str.to_string()
                                        });

                                    handle.run_record.stderr_bytes = stderr_bytes;
                                    handle.run_record.stderr_truncated =
                                        stderr_bytes > MAX_INLINE_OUTPUT_BYTES as u64;
                                    handle.run_record.stderr_preview =
                                        Some(if stderr_str.len() > MAX_INLINE_OUTPUT_BYTES {
                                            stderr_str[..MAX_INLINE_OUTPUT_BYTES].to_string()
                                        } else {
                                            stderr_str.to_string()
                                        });
                                }

                                self.store.record_service_run(&handle.run_record).await.ok();
                                active_services.push(handle);
                                let env_st = if is_timeout {
                                    EnvironmentRunStatus::TimedOut
                                } else {
                                    EnvironmentRunStatus::Error
                                };
                                environment_err = Some((env_st, err_str));
                                break;
                            }

                            let ready_at_ms = crate::verification::now_millis();
                            handle.run_record.ready_at_ms = Some(ready_at_ms);
                            handle.run_record.status = EnvironmentServiceStatus::Ready;
                            self.store.record_service_run(&handle.run_record).await.ok();
                            active_services.push(handle);
                        }
                        Ok(out) => {
                            let err_msg = format!(
                                "failed to start process service '{}': {}",
                                svc.id,
                                String::from_utf8_lossy(&out.stderr)
                            );
                            let srec = EnvironmentServiceRun {
                                id: s_run_id,
                                environment_run_id: env_run.id.clone(),
                                service_id: svc.id.clone(),
                                service_kind: svc.kind,
                                status: EnvironmentServiceStatus::Error,
                                image_ref: Some(image_ref.to_string()),
                                resolved_image_digest: resolved_digest,
                                container_name: Some(cname),
                                host_port: None,
                                internal_port: svc.internal_port,
                                started_at_ms,
                                ready_at_ms: None,
                                finished_at_ms: Some(crate::verification::now_millis()),
                                duration_ms: Some(0),
                                stdout_preview: None,
                                stdout_truncated: false,
                                stdout_bytes: 0,
                                stderr_preview: Some(String::from_utf8_lossy(&out.stderr).into()),
                                stderr_truncated: false,
                                stderr_bytes: out.stderr.len() as u64,
                                error_message: Some(err_msg.clone()),
                            };
                            self.store.record_service_run(&srec).await.ok();
                            environment_err = Some((EnvironmentRunStatus::Error, err_msg));
                            break;
                        }
                        Err(e) => {
                            let err_msg = format!("failed to spawn podman: {e}");
                            let srec = EnvironmentServiceRun {
                                id: s_run_id,
                                environment_run_id: env_run.id.clone(),
                                service_id: svc.id.clone(),
                                service_kind: svc.kind,
                                status: EnvironmentServiceStatus::Error,
                                image_ref: Some(image_ref.to_string()),
                                resolved_image_digest: resolved_digest,
                                container_name: Some(cname),
                                host_port: None,
                                internal_port: svc.internal_port,
                                started_at_ms,
                                ready_at_ms: None,
                                finished_at_ms: Some(crate::verification::now_millis()),
                                duration_ms: Some(0),
                                stdout_preview: None,
                                stdout_truncated: false,
                                stdout_bytes: 0,
                                stderr_preview: None,
                                stderr_truncated: false,
                                stderr_bytes: 0,
                                error_message: Some(err_msg.clone()),
                            };
                            self.store.record_service_run(&srec).await.ok();
                            environment_err = Some((EnvironmentRunStatus::Error, err_msg));
                            break;
                        }
                    }
                }
            }
        }

        // 4. If environment services failed to start, cleanup and return environment error
        if let Some((err_status, err_msg)) = environment_err {
            Self::teardown_services(&self.store, active_services, network_name.as_deref()).await;
            self.store
                .finalize_environment_run(&env_run.id, err_status, Some(&err_msg))
                .await?;
            let completed_env = self.store.get_environment_run(&env_run.id).await?.unwrap();
            return Ok((completed_env, Err((err_status, err_msg))));
        }

        // 5. Environment is ready!
        self.store.mark_environment_ready(&env_run.id).await?;

        // 6. Run setup steps if any
        let mut setup_failed = false;
        let mut setup_err = String::new();
        for step in &spec.setup_steps {
            let cap_res = crate::verification::execute_verification_command_isolated(
                step,
                workspace_dir,
                None,
                cancellation_token.clone(),
                default_runtime_image,
                None,
            )
            .await;
            match cap_res {
                Ok(cap) if cap.exit_code == Some(0) => {}
                Ok(cap) => {
                    setup_failed = true;
                    setup_err = format!(
                        "setup step '{}' failed with exit code {:?}",
                        step.name, cap.exit_code
                    );
                    break;
                }
                Err(e) => {
                    setup_failed = true;
                    setup_err = format!("setup step '{}' error: {e}", step.name);
                    break;
                }
            }
        }

        if setup_failed {
            Self::teardown_services(&self.store, active_services, network_name.as_deref()).await;
            self.store
                .finalize_environment_run(
                    &env_run.id,
                    EnvironmentRunStatus::Failed,
                    Some(&setup_err),
                )
                .await?;
            let completed_env = self.store.get_environment_run(&env_run.id).await?.unwrap();
            return Ok((
                completed_env,
                Err((EnvironmentRunStatus::Failed, setup_err)),
            ));
        }

        // 7. Execute tests via caller's callback (passing network_name so test containers join network)
        let test_result = test_execution_fn(network_name.clone()).await;

        // 8. Teardown active services in reverse dependency order
        Self::teardown_services(&self.store, active_services, network_name.as_deref()).await;

        // 9. Finalize environment run record
        let final_env_status = match &test_result {
            Ok(_) => EnvironmentRunStatus::Passed,
            Err((st, _)) => *st,
        };
        let final_err_msg = match &test_result {
            Ok(_) => None,
            Err((_, msg)) => Some(msg.as_str()),
        };
        self.store
            .finalize_environment_run(&env_run.id, final_env_status, final_err_msg)
            .await?;

        let completed_env = self.store.get_environment_run(&env_run.id).await?.unwrap();
        Ok((completed_env, test_result))
    }

    /// Teardown services in reverse order and prune isolated network.
    pub async fn teardown_services(
        store: &EnvironmentStore,
        mut active_services: Vec<ActiveServiceHandle>,
        network_name: Option<&str>,
    ) {
        while let Some(mut handle) = active_services.pop() {
            if let Some(ref cname) = handle.container_name {
                let mut logs_cmd = tokio::process::Command::new("podman");
                logs_cmd.args(["--remote=false", "--cgroup-manager=cgroupfs", "logs", cname]);
                if let Ok(lout) = logs_cmd.output().await {
                    let stdout_str = String::from_utf8_lossy(&lout.stdout);
                    let stderr_str = String::from_utf8_lossy(&lout.stderr);
                    let stdout_bytes = lout.stdout.len() as u64;
                    let stderr_bytes = lout.stderr.len() as u64;

                    handle.run_record.stdout_bytes = stdout_bytes;
                    handle.run_record.stdout_truncated =
                        stdout_bytes > MAX_INLINE_OUTPUT_BYTES as u64;
                    handle.run_record.stdout_preview =
                        Some(if stdout_str.len() > MAX_INLINE_OUTPUT_BYTES {
                            stdout_str[..MAX_INLINE_OUTPUT_BYTES].to_string()
                        } else {
                            stdout_str.to_string()
                        });

                    handle.run_record.stderr_bytes = stderr_bytes;
                    handle.run_record.stderr_truncated =
                        stderr_bytes > MAX_INLINE_OUTPUT_BYTES as u64;
                    handle.run_record.stderr_preview =
                        Some(if stderr_str.len() > MAX_INLINE_OUTPUT_BYTES {
                            stderr_str[..MAX_INLINE_OUTPUT_BYTES].to_string()
                        } else {
                            stderr_str.to_string()
                        });
                }

                let now = crate::verification::now_millis();
                handle.run_record.finished_at_ms = Some(now);
                handle.run_record.duration_ms = Some(now - handle.run_record.started_at_ms);

                store.record_service_run(&handle.run_record).await.ok();

                let mut rm_cmd = tokio::process::Command::new("podman");
                rm_cmd.args([
                    "--remote=false",
                    "--cgroup-manager=cgroupfs",
                    "rm",
                    "-f",
                    cname,
                ]);
                let _ = rm_cmd.output().await;
            }

            if let Some(ref mut child) = handle.child {
                let _ = child.kill().await;
            }
            drop(handle.pid_guard);
            if let Some(task) = handle.stdout_task {
                let _ = task.await;
            }
            if let Some(task) = handle.stderr_task {
                let _ = task.await;
            }
        }

        if let Some(net) = network_name {
            let mut net_rm = tokio::process::Command::new("podman");
            net_rm.args([
                "--remote=false",
                "--cgroup-manager=cgroupfs",
                "network",
                "rm",
                net,
            ]);
            let _ = net_rm.output().await;
        }
    }
}
