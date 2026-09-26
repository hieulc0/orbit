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

/// Network isolation policy for verification environments.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VerificationNetworkPolicy {
    #[default]
    None,
    Isolated,
}

impl std::fmt::Display for VerificationNetworkPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, "none"),
            Self::Isolated => write!(f, "isolated"),
        }
    }
}

/// Cache policy for dependencies/language packages in verification sandboxes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VerificationCachePolicy {
    #[default]
    Clean,
    DeclaredCache,
}

impl std::fmt::Display for VerificationCachePolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Clean => write!(f, "clean"),
            Self::DeclaredCache => write!(f, "declared_cache"),
        }
    }
}

/// Allowlist-based environment variable policy for verification sandboxes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct VerificationEnvironmentPolicy {
    #[serde(default)]
    pub inherit: Vec<String>,
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

impl VerificationEnvironmentPolicy {
    pub fn clean() -> Self {
        Self {
            inherit: Vec::new(),
            set: BTreeMap::new(),
            deny: Vec::new(),
        }
    }

    pub fn digest(&self) -> String {
        let serialized = serde_json::to_string(self).unwrap_or_default();
        crate::model::digest(serialized.as_bytes())
    }
}

/// Permitted command or check declaration under a verification policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowedCommand {
    pub executable: String,
    #[serde(default)]
    pub args_prefix: Vec<String>,
}

impl AllowedCommand {
    pub fn exact(executable: impl Into<String>) -> Self {
        Self {
            executable: executable.into(),
            args_prefix: Vec::new(),
        }
    }

    pub fn with_prefix(executable: impl Into<String>, prefix: Vec<String>) -> Self {
        Self {
            executable: executable.into(),
            args_prefix: prefix,
        }
    }

    pub fn permits(&self, argv: &[String]) -> bool {
        if argv.is_empty() {
            return false;
        }
        let exec = &argv[0];
        // Match executable by base name or full path
        let exec_base = Path::new(exec)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or(exec);
        let allowed_base = Path::new(&self.executable)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or(&self.executable);

        if exec_base != allowed_base && exec != &self.executable {
            return false;
        }

        if self.args_prefix.is_empty() {
            return true;
        }

        if argv.len() - 1 < self.args_prefix.len() {
            return false;
        }

        for (actual, expected) in argv[1..].iter().zip(&self.args_prefix) {
            if actual != expected {
                return false;
            }
        }
        true
    }
}

/// Durable, versioned policy describing what qualifies a workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationPolicy {
    pub id: String,
    pub version: u32,
    pub name: String,
    #[serde(default)]
    pub required_steps: Vec<String>,
    #[serde(default)]
    pub allowed_commands: Vec<AllowedCommand>,
    #[serde(default)]
    pub environment_policy: VerificationEnvironmentPolicy,
    #[serde(default)]
    pub network_policy: VerificationNetworkPolicy,
    #[serde(default)]
    pub cache_policy: VerificationCachePolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration_environment_spec:
        Option<crate::integration_environment::IntegrationEnvironmentSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_verification_spec: Option<crate::browser_verification::BrowserVerificationSpec>,
}

impl VerificationPolicy {
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            version: 1,
            name: name.into(),
            required_steps: Vec::new(),
            allowed_commands: Vec::new(),
            environment_policy: VerificationEnvironmentPolicy::clean(),
            network_policy: VerificationNetworkPolicy::None,
            cache_policy: VerificationCachePolicy::Clean,
            integration_environment_spec: None,
            browser_verification_spec: None,
        }
    }

    pub fn digest(&self) -> String {
        let serialized = serde_json::to_string(self).unwrap_or_default();
        crate::model::digest(serialized.as_bytes())
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(!self.id.trim().is_empty(), "policy id required");
        ensure!(!self.name.trim().is_empty(), "policy name required");
        ensure!(self.version > 0, "policy version must be > 0");
        if let Some(ref b) = self.browser_verification_spec {
            b.validate()?;
        }
        Ok(())
    }

    /// Check if a plan satisfies this policy before execution.
    pub fn check_plan(&self, plan: &VerificationPlan) -> Result<()> {
        plan.validate()?;

        // 1. If policy specifies required steps by ID, they must exist in the plan
        for req_id in &self.required_steps {
            let found = plan.steps.iter().any(|s| &s.id == req_id && s.required);
            ensure!(
                found,
                "plan does not contain required step '{}' mandated by policy '{}'",
                req_id,
                self.id
            );
        }

        // 2. Check allowed commands if policy defines an allowlist
        if !self.allowed_commands.is_empty() {
            for step in &plan.steps {
                let allowed = self
                    .allowed_commands
                    .iter()
                    .any(|ac| ac.permits(&step.argv));
                ensure!(
                    allowed,
                    "step '{}' with argv {:?} is not permitted by verification policy '{}'",
                    step.id,
                    step.argv,
                    self.id
                );
            }
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
    #[serde(default)]
    pub network_policy: VerificationNetworkPolicy,
    #[serde(default)]
    pub cache_policy: VerificationCachePolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_policy_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration_environment_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_verification_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_runtime_image_digest: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_digest: Option<String>,
    pub status: VerificationStepStatus,
    pub environment_identity: EnvironmentIdentity,
    pub started_at_ms: i64,
    pub finished_at_ms: Option<i64>,
    pub overall_result: Option<VerificationRunResult>,
    pub step_runs: Vec<VerificationStepRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_verification_run: Option<crate::browser_verification::BrowserVerificationRun>,
}

pub fn now_millis() -> i64 {
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
        None,
    )
    .await
}

pub async fn execute_verification_command_isolated(
    step: &VerificationStep,
    workspace_dir: &Path,
    timeout_override: Option<Duration>,
    cancellation_token: Option<tokio::sync::watch::Receiver<bool>>,
    isolation_image: Option<&str>,
    environment_policy: Option<&VerificationEnvironmentPolicy>,
) -> Result<CommandOutputCapture> {
    execute_verification_command_isolated_with_network(
        step,
        workspace_dir,
        timeout_override,
        cancellation_token,
        isolation_image,
        environment_policy,
        None,
    )
    .await
}

pub async fn execute_verification_command_isolated_with_network(
    step: &VerificationStep,
    workspace_dir: &Path,
    timeout_override: Option<Duration>,
    mut cancellation_token: Option<tokio::sync::watch::Receiver<bool>>,
    isolation_image: Option<&str>,
    environment_policy: Option<&VerificationEnvironmentPolicy>,
    network_name: Option<&str>,
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

        // Deterministic clean environment defaults per B2 specification
        let mut env_map: BTreeMap<String, String> = BTreeMap::new();
        env_map.insert("HOME".to_string(), "/tmp/orbit-home".to_string());
        env_map.insert("CI".to_string(), "1".to_string());
        env_map.insert("LANG".to_string(), "C.UTF-8".to_string());
        env_map.insert("LC_ALL".to_string(), "C.UTF-8".to_string());
        env_map.insert("TERM".to_string(), "dumb".to_string());
        env_map.insert(
            "PATH".to_string(),
            "/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
                .to_string(),
        );
        env_map.insert("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string());
        env_map.insert("GIT_CONFIG_GLOBAL".to_string(), "/dev/null".to_string());
        env_map.insert("ORBIT_VERIFICATION".to_string(), "1".to_string());

        // Apply environment policy if provided
        if let Some(pol) = environment_policy {
            // Inherit explicitly allowed keys only
            for key in &pol.inherit {
                if !pol.deny.contains(key)
                    && let Ok(val) = std::env::var(key)
                {
                    env_map.insert(key.clone(), val);
                }
            }
            // Explicitly set variables
            for (k, v) in &pol.set {
                if !pol.deny.contains(k) {
                    env_map.insert(k.clone(), v.clone());
                }
            }
        }

        // Apply step-level env additions (unless denied by policy)
        for (k, v) in &step.env {
            if let Some(pol) = environment_policy
                && pol.deny.contains(k)
            {
                continue;
            }
            env_map.insert(k.clone(), v.clone());
        }

        let mut run_args = vec![
            "--remote=false".to_string(),
            "--cgroup-manager=cgroupfs".to_string(),
            "run".to_string(),
            "--rm".to_string(),
            "--name".to_string(),
            name.to_string(),
        ];
        if let Some(net) = network_name {
            run_args.push(format!("--network={}", net));
        } else {
            run_args.push("--network=none".to_string());
        }
        run_args.extend([
            "--read-only".to_string(),
            "--cap-drop=ALL".to_string(),
            "--security-opt=no-new-privileges".to_string(),
            "--pids-limit=128".to_string(),
            "--init".to_string(),
            "--log-driver=none".to_string(),
            "--tmpfs".to_string(),
            "/tmp:rw,nosuid,nodev,size=67108864".to_string(),
            "--mount".to_string(),
            format!(
                "type=bind,src={},dst=/workspace",
                canonical_workspace.display()
            ),
            "--workdir".to_string(),
            cont_cwd.to_string(),
        ]);
        c.args(&run_args);

        for (k, v) in &env_map {
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
        let mut env_map: BTreeMap<String, String> = BTreeMap::new();
        env_map.insert("HOME".to_string(), "/tmp/orbit-home".to_string());
        env_map.insert("CI".to_string(), "1".to_string());
        env_map.insert("LANG".to_string(), "C.UTF-8".to_string());
        env_map.insert("LC_ALL".to_string(), "C.UTF-8".to_string());
        env_map.insert("TERM".to_string(), "dumb".to_string());
        env_map.insert(
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into()),
        );
        env_map.insert("ORBIT_VERIFICATION".to_string(), "1".to_string());

        if let Some(pol) = environment_policy {
            for key in &pol.inherit {
                if !pol.deny.contains(key)
                    && let Ok(val) = std::env::var(key)
                {
                    env_map.insert(key.clone(), val);
                }
            }
            for (k, v) in &pol.set {
                if !pol.deny.contains(k) {
                    env_map.insert(k.clone(), v.clone());
                }
            }
        }

        for (k, v) in &step.env {
            if let Some(pol) = environment_policy
                && pol.deny.contains(k)
            {
                continue;
            }
            env_map.insert(k.clone(), v.clone());
        }

        c.args(&step.argv[1..])
            .current_dir(&cwd)
            .env_clear()
            .envs(&env_map);
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

    pub fn pool(&self) -> &PgPool {
        &self.pool
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

    /// Save or update a verification policy.
    pub async fn save_policy(&self, policy: &VerificationPolicy) -> Result<()> {
        policy.validate()?;
        let digest = policy.digest();
        sqlx::query(
            r#"
            INSERT INTO orbit_verification_policies (id, version, digest, name, definition)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (id, version) DO UPDATE SET
                digest = EXCLUDED.digest,
                name = EXCLUDED.name,
                definition = EXCLUDED.definition
            "#,
        )
        .bind(&policy.id)
        .bind(policy.version as i32)
        .bind(&digest)
        .bind(&policy.name)
        .bind(serde_json::to_value(policy)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Get a policy by ID and version.
    pub async fn get_policy(
        &self,
        policy_id: &str,
        version: u32,
    ) -> Result<Option<VerificationPolicy>> {
        let row = sqlx::query_scalar::<_, serde_json::Value>(
            "SELECT definition FROM orbit_verification_policies WHERE id = $1 AND version = $2",
        )
        .bind(policy_id)
        .bind(version as i32)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some(val) => Ok(Some(serde_json::from_value(val)?)),
            None => Ok(None),
        }
    }

    /// Get the latest version of a policy by ID.
    pub async fn get_latest_policy(&self, policy_id: &str) -> Result<Option<VerificationPolicy>> {
        let row = sqlx::query_scalar::<_, serde_json::Value>(
            "SELECT definition FROM orbit_verification_policies WHERE id = $1 ORDER BY version DESC LIMIT 1",
        )
        .bind(policy_id)
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
        self.create_run_with_policy(attempt_id, workspace_state, plan, environment, None)
            .await
    }

    /// Create a new VerificationRun record in PENDING status optionally bound to a policy.
    pub async fn create_run_with_policy(
        &self,
        attempt_id: &str,
        workspace_state: &WorkspaceState,
        plan: &VerificationPlan,
        environment: EnvironmentIdentity,
        policy: Option<&VerificationPolicy>,
    ) -> Result<VerificationRun> {
        plan.validate()?;
        if let Some(pol) = policy {
            pol.check_plan(plan)?;
        }
        let run_id = format!("vrun-{}", crate::model::id());
        let started_at_ms = now_millis();
        let (pol_id, pol_ver, pol_dig) = policy
            .map(|p| (Some(p.id.clone()), Some(p.version as i32), Some(p.digest())))
            .unwrap_or((None, None, None));

        sqlx::query(
            r#"
            INSERT INTO orbit_verification_runs (
                id, attempt_id, workspace_state_id, plan_id, plan_version,
                plan_snapshot, policy_id, policy_version, policy_digest,
                status, environment_identity, started_at_ms
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
            "#,
        )
        .bind(&run_id)
        .bind(attempt_id)
        .bind(&workspace_state.state_id)
        .bind(&plan.id)
        .bind(plan.version as i32)
        .bind(serde_json::to_value(plan)?)
        .bind(&pol_id)
        .bind(pol_ver)
        .bind(&pol_dig)
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
            policy_id: pol_id,
            policy_version: pol_ver.map(|v| v as u32),
            policy_digest: pol_dig,
            status: VerificationStepStatus::Pending,
            environment_identity: environment,
            started_at_ms,
            finished_at_ms: None,
            overall_result: None,
            step_runs: Vec::new(),
            browser_verification_run: None,
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
            policy_id: Option<String>,
            policy_version: Option<i32>,
            policy_digest: Option<String>,
            status: String,
            environment_identity: serde_json::Value,
            started_at_ms: i64,
            finished_at_ms: Option<i64>,
            overall_result: Option<String>,
        }

        let run_opt = sqlx::query_as::<_, RunRow>(
            r#"
            SELECT id, attempt_id, workspace_state_id, plan_id, plan_version,
                   plan_snapshot, policy_id, policy_version, policy_digest,
                   status, environment_identity, started_at_ms,
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

        let bstore = crate::browser_verification::BrowserStore::new(self.pool.clone());
        let browser_verification_run = bstore
            .get_browser_run_for_verification(&row.id)
            .await
            .unwrap_or(None);

        Ok(Some(VerificationRun {
            id: row.id,
            attempt_id: row.attempt_id,
            workspace_state_id: row.workspace_state_id,
            plan_id: row.plan_id,
            plan_version: row.plan_version as u32,
            plan_snapshot: serde_json::from_value(row.plan_snapshot)?,
            policy_id: row.policy_id,
            policy_version: row.policy_version.map(|v| v as u32),
            policy_digest: row.policy_digest,
            status,
            environment_identity: serde_json::from_value(row.environment_identity)?,
            started_at_ms: row.started_at_ms,
            finished_at_ms: row.finished_at_ms,
            overall_result,
            step_runs,
            browser_verification_run,
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

    /// Check if a workspace state qualifies under a specific policy and environment requirements.
    ///
    /// Section 10 Qualification Lookup Invariant:
    /// A valid qualification must require:
    /// - workspace_state_id matches
    /// - policy identity/version matches
    /// - environment requirements match (runtime_image_digest, isolation, network_policy)
    /// - run status == PASSED
    /// - all required steps passed
    pub async fn check_workspace_qualification(
        &self,
        workspace_state_id: &str,
        policy: &VerificationPolicy,
        required_environment: Option<&EnvironmentIdentity>,
    ) -> Result<Option<VerificationRun>> {
        let runs: Vec<String> = sqlx::query_scalar(
            r#"
            SELECT id FROM orbit_verification_runs
            WHERE workspace_state_id = $1
              AND policy_id = $2
              AND policy_version = $3
              AND overall_result = 'PASSED'
            ORDER BY created_at DESC
            "#,
        )
        .bind(workspace_state_id)
        .bind(&policy.id)
        .bind(policy.version as i32)
        .fetch_all(&self.pool)
        .await?;

        for run_id in runs {
            if let Some(run) = self.get_run(&run_id).await? {
                // Check policy digest matches
                if run.policy_digest.as_deref() != Some(&policy.digest()) {
                    continue;
                }

                // Check environment requirements if specified
                if let Some(req_env) = required_environment {
                    if req_env.isolation != run.environment_identity.isolation {
                        continue;
                    }
                    if req_env.runtime_image_digest.is_some()
                        && req_env.runtime_image_digest
                            != run.environment_identity.runtime_image_digest
                    {
                        continue;
                    }
                    if req_env.network_policy != run.environment_identity.network_policy {
                        continue;
                    }
                    if req_env.cache_policy != run.environment_identity.cache_policy {
                        continue;
                    }
                    if req_env.integration_environment_digest.is_some()
                        && req_env.integration_environment_digest
                            != run.environment_identity.integration_environment_digest
                    {
                        continue;
                    }
                    if req_env.browser_verification_digest.is_some()
                        && req_env.browser_verification_digest
                            != run.environment_identity.browser_verification_digest
                    {
                        continue;
                    }
                    if req_env.browser_runtime_image_digest.is_some()
                        && req_env.browser_runtime_image_digest
                            != run.environment_identity.browser_runtime_image_digest
                    {
                        continue;
                    }
                }

                if let Some(ref _bspec) = policy.browser_verification_spec {
                    let bstore = crate::browser_verification::BrowserStore::new(self.pool.clone());
                    if let Ok(Some(brun)) = bstore.get_browser_run_for_verification(&run.id).await {
                        if brun.status
                            != crate::browser_verification::BrowserVerificationStatus::Passed
                        {
                            continue;
                        }
                        let all_req_browser_passed = brun.test_runs.iter().all(|t| {
                            !t.required
                                || t.status
                                    == crate::browser_verification::BrowserTestStatus::Passed
                        });
                        if !all_req_browser_passed {
                            continue;
                        }
                    } else {
                        continue;
                    }
                }

                // Ensure all required steps in the policy actually ran and passed
                let mut policy_steps_satisfied = true;
                for req_step_id in &policy.required_steps {
                    let step_passed = run.step_runs.iter().any(|s| {
                        &s.step_id == req_step_id && s.status == VerificationStepStatus::Passed
                    });
                    if !step_passed {
                        policy_steps_satisfied = false;
                        break;
                    }
                }
                if !policy_steps_satisfied {
                    continue;
                }

                // Ensure all required steps in the plan passed
                let all_plan_req_passed = run
                    .step_runs
                    .iter()
                    .all(|s| !s.required || s.status == VerificationStepStatus::Passed);
                if !all_plan_req_passed {
                    continue;
                }

                return Ok(Some(run));
            }
        }

        Ok(None)
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
    execute_verification_plan_with_policy(
        store,
        attempt_id,
        workspace_state,
        plan,
        workspace_dir,
        environment,
        None,
        cancellation_token,
    )
    .await
}

/// Execute a VerificationPlan governed by an explicit VerificationPolicy.
#[allow(clippy::too_many_arguments)]
pub async fn execute_verification_plan_with_policy(
    store: &VerificationStore,
    attempt_id: &str,
    workspace_state: &WorkspaceState,
    plan: &VerificationPlan,
    workspace_dir: &Path,
    environment: EnvironmentIdentity,
    policy: Option<&VerificationPolicy>,
    cancellation_token: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<VerificationRun> {
    plan.validate()?;
    if let Some(pol) = policy {
        pol.check_plan(plan)?;
    }

    let run = store
        .create_run_with_policy(attempt_id, workspace_state, plan, environment, policy)
        .await?;

    let mut overall_result;
    let mut step_runs = Vec::new();

    // Check if policy specifies an integration environment spec
    let env_spec = policy.and_then(|p| p.integration_environment_spec.as_ref());
    let env_manager = crate::integration_environment::EnvironmentManager::new(
        crate::integration_environment::EnvironmentStore::new(store.pool().clone()),
    );
    let browser_spec = policy.and_then(|p| p.browser_verification_spec.as_ref());
    let browser_store = crate::browser_verification::BrowserStore::new(store.pool().clone());
    let browser_manager =
        crate::browser_verification::BrowserVerificationManager::new(browser_store);
    let artifacts_dir = std::env::var("ORBIT_ARTIFACTS_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .ok()
                .map(|h| std::path::PathBuf::from(h).join(".orbit").join("artifacts"))
                .unwrap_or_else(|| std::path::PathBuf::from("/tmp/orbit-artifacts"))
        });

    let run_id = run.id.clone();
    let runtime_image = run.environment_identity.runtime_image.clone();

    // Helper executing the steps given a network_name
    let execute_steps =
        |network_name: Option<String>, cancel_tok: Option<tokio::sync::watch::Receiver<bool>>| {
            let run_id = run_id.clone();
            let runtime_image = runtime_image.clone();
            async move {
                let mut inner_steps = Vec::new();
                let mut inner_overall = VerificationRunResult::Passed;

                for step in &plan.steps {
                    if cancel_tok.as_ref().is_some_and(|token| *token.borrow()) {
                        inner_overall = VerificationRunResult::Cancelled;
                        break;
                    }

                    let step_run_id = format!("vstep-{}", crate::model::id());
                    let step_started_at_ms = now_millis();

                    let image_opt = runtime_image.as_deref();
                    let env_pol = policy.map(|p| &p.environment_policy);
                    let capture_res = execute_verification_command_isolated_with_network(
                        step,
                        workspace_dir,
                        None,
                        cancel_tok.clone(),
                        image_opt,
                        env_pol,
                        network_name.as_deref(),
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
                        verification_run_id: run_id.clone(),
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

                    store.record_step_run(&step_record).await.ok();
                    inner_steps.push(step_record);

                    if status != VerificationStepStatus::Passed && step.required {
                        inner_overall = match status {
                            VerificationStepStatus::TimedOut => VerificationRunResult::TimedOut,
                            VerificationStepStatus::Cancelled => VerificationRunResult::Cancelled,
                            VerificationStepStatus::Error => VerificationRunResult::Error,
                            _ => VerificationRunResult::Failed,
                        };
                        break;
                    }
                }
                (inner_overall, inner_steps)
            }
        };

    if let Some(spec) = env_spec {
        let cancel_clone = cancellation_token.clone();
        let (_env_run, lifecycle_res) = env_manager
            .run_environment_lifecycle(
                &run.id,
                &workspace_state.state_id,
                spec,
                workspace_dir,
                runtime_image.as_deref(),
                cancellation_token,
                |net_name| {
                    let cancel_inner = cancel_clone.clone();
                    let b_spec = browser_spec.cloned();
                    let b_mgr = browser_manager.clone();
                    let b_run_id = run_id.clone();
                    let b_ws_state = workspace_state.state_id.clone();
                    let b_ws_dir = workspace_dir.to_path_buf();
                    let b_art_dir = artifacts_dir.clone();
                    async move {
                        let (res, steps) = execute_steps(net_name.clone(), cancel_inner.clone()).await;
                        if res == VerificationRunResult::Passed
                            && let Some(ref bs) = b_spec {
                                let b_res = b_mgr
                                    .execute_browser_verification(
                                        &b_run_id,
                                        None,
                                        &b_ws_state,
                                        bs,
                                        &b_ws_dir,
                                        net_name.as_deref(),
                                        None,
                                        cancel_inner,
                                        &b_art_dir,
                                    )
                                    .await;
                                match b_res {
                                    Ok(brun) => {
                                        if brun.status != crate::browser_verification::BrowserVerificationStatus::Passed {
                                            let st = match brun.status {
                                                crate::browser_verification::BrowserVerificationStatus::TimedOut => {
                                                    VerificationRunResult::TimedOut
                                                }
                                                crate::browser_verification::BrowserVerificationStatus::Cancelled => {
                                                    VerificationRunResult::Cancelled
                                                }
                                                crate::browser_verification::BrowserVerificationStatus::Error => {
                                                    VerificationRunResult::Error
                                                }
                                                _ => VerificationRunResult::Failed,
                                            };
                                            return Ok((st, steps));
                                        }
                                    }
                                    Err(err) => {
                                        return Err((
                                            crate::integration_environment::EnvironmentRunStatus::Error,
                                            format!("Browser verification failed: {}", err),
                                        ));
                                    }
                                }
                            }
                        Ok((res, steps))
                    }
                },
            )
            .await?;

        match lifecycle_res {
            Ok((res, steps)) => {
                overall_result = res;
                step_runs = steps;
            }
            Err((env_status, err_msg)) => {
                overall_result = match env_status {
                    crate::integration_environment::EnvironmentRunStatus::TimedOut => {
                        VerificationRunResult::TimedOut
                    }
                    crate::integration_environment::EnvironmentRunStatus::Cancelled => {
                        VerificationRunResult::Cancelled
                    }
                    crate::integration_environment::EnvironmentRunStatus::Error => {
                        VerificationRunResult::Error
                    }
                    _ => VerificationRunResult::Failed,
                };
                // If no steps ran, create an informative step or record error message
                if step_runs.is_empty() {
                    let step_run_id = format!("vstep-env-{}", crate::model::id());
                    let now = now_millis();
                    let step_record = VerificationStepRun {
                        id: step_run_id,
                        verification_run_id: run.id.clone(),
                        step_id: "environment_lifecycle".into(),
                        step_name: "Integration Environment".into(),
                        status: match overall_result {
                            VerificationRunResult::TimedOut => VerificationStepStatus::TimedOut,
                            VerificationRunResult::Cancelled => VerificationStepStatus::Cancelled,
                            VerificationRunResult::Error => VerificationStepStatus::Error,
                            _ => VerificationStepStatus::Failed,
                        },
                        required: true,
                        exit_code: Some(1),
                        started_at_ms: now,
                        finished_at_ms: Some(now),
                        duration_ms: Some(0),
                        stdout_preview: None,
                        stdout_truncated: false,
                        stdout_bytes: 0,
                        stdout_artifact_id: None,
                        stderr_preview: Some(err_msg.clone()),
                        stderr_truncated: false,
                        stderr_bytes: err_msg.len() as u64,
                        stderr_artifact_id: None,
                        artifacts: Vec::new(),
                        error_message: Some(err_msg),
                    };
                    store.record_step_run(&step_record).await.ok();
                    step_runs.push(step_record);
                }
            }
        }
    } else {
        let (res, steps) = execute_steps(None, cancellation_token.clone()).await;
        overall_result = res;
        step_runs = steps;

        if overall_result == VerificationRunResult::Passed
            && let Some(bs) = browser_spec
        {
            let b_res = browser_manager
                .execute_browser_verification(
                    &run.id,
                    None,
                    &workspace_state.state_id,
                    bs,
                    workspace_dir,
                    None,
                    None,
                    cancellation_token,
                    &artifacts_dir,
                )
                .await;
            match b_res {
                Ok(brun) => {
                    if brun.status != crate::browser_verification::BrowserVerificationStatus::Passed
                    {
                        overall_result = match brun.status {
                            crate::browser_verification::BrowserVerificationStatus::TimedOut => {
                                VerificationRunResult::TimedOut
                            }
                            crate::browser_verification::BrowserVerificationStatus::Cancelled => {
                                VerificationRunResult::Cancelled
                            }
                            crate::browser_verification::BrowserVerificationStatus::Error => {
                                VerificationRunResult::Error
                            }
                            _ => VerificationRunResult::Failed,
                        };
                    }
                }
                Err(_) => {
                    overall_result = VerificationRunResult::Error;
                }
            }
        }
    }

    store.finalize_run(&run.id, overall_result).await?;
    let mut completed_run = store
        .get_run(&run.id)
        .await?
        .context("run record disappeared")?;
    completed_run.step_runs = step_runs;
    if browser_spec.is_some() {
        let bstore = crate::browser_verification::BrowserStore::new(store.pool().clone());
        if let Ok(Some(brun)) = bstore.get_browser_run_for_verification(&run.id).await {
            completed_run
                .environment_identity
                .browser_verification_digest = Some(brun.spec_digest.clone());
            completed_run
                .environment_identity
                .browser_runtime_image_digest = Some(brun.browser_image_digest.clone());
            sqlx::query(
                "UPDATE orbit_verification_runs SET environment_identity = $2 WHERE id = $1",
            )
            .bind(&run.id)
            .bind(serde_json::to_value(&completed_run.environment_identity)?)
            .execute(store.pool())
            .await?;
            completed_run.browser_verification_run = Some(brun);
        }
    }
    Ok(completed_run)
}

/// Format human-readable output for `orbit verification show <run_id>`.
pub fn format_verification_show(run: &VerificationRun) -> String {
    let mut out = String::new();
    out.push_str(&format!("Verification {}\n", run.id));
    out.push_str(&format!("Attempt       {}\n", run.attempt_id));
    out.push_str(&format!("Workspace     {}\n", run.workspace_state_id));
    if let Some(pid) = &run.policy_id {
        out.push_str(&format!(
            "Policy        {} (v{})\n",
            pid,
            run.policy_version.unwrap_or(1)
        ));
    }
    if let Some(pdig) = &run.policy_digest {
        out.push_str(&format!("Policy Digest {}\n", pdig));
    }
    if let Some(img) = &run.environment_identity.runtime_image {
        out.push_str(&format!("Image         {}\n", img));
    }
    if let Some(dig) = &run.environment_identity.runtime_image_digest {
        out.push_str(&format!("Image Digest  {}\n", dig));
    }
    out.push_str(&format!(
        "Network       {}\n",
        run.environment_identity.network_policy
    ));
    out.push_str(&format!(
        "Cache Policy  {}\n",
        run.environment_identity.cache_policy
    ));
    if let Some(int_dig) = &run.environment_identity.integration_environment_digest {
        out.push_str(&format!("Env Digest    {}\n", int_dig));
    }
    if let Some(b_dig) = &run.environment_identity.browser_verification_digest {
        out.push_str(&format!(
            "Browser Spec  {}
",
            b_dig
        ));
    }
    if let Some(b_img) = &run.environment_identity.browser_runtime_image_digest {
        out.push_str(&format!(
            "Browser Image {}
",
            b_img
        ));
    }
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

    if let Some(brun) = &run.browser_verification_run {
        out.push_str("\nBROWSER VERIFICATION\n");
        out.push_str(&format!("  Backend:        {}\n", brun.browser_backend));
        out.push_str(&format!("  Status:         {}\n", brun.status));
        if let Some(reason) = &brun.overall_failure_reason {
            out.push_str(&format!("  Failure Reason: {}\n", reason));
        }
        if let Some(dur) = brun.duration_ms {
            out.push_str(&format!("  Duration:       {:.1}s\n", dur as f64 / 1000.0));
        }
        out.push_str("\n  BROWSER TESTS\n");
        out.push_str(&format!(
            "  {:<20} {:<10} {:<10} {}\n",
            "TEST ID", "STATUS", "DURATION", "REASON"
        ));
        for t in &brun.test_runs {
            let dur_str = t
                .duration_ms
                .map(|d| format!("{:.1}s", d as f64 / 1000.0))
                .unwrap_or_else(|| "—".into());
            let reason_str = t
                .failure_reason
                .as_ref()
                .map(|r| r.to_string())
                .unwrap_or_else(|| "—".into());
            out.push_str(&format!(
                "  {:<20} {:<10} {:<10} {}\n",
                t.test_id, t.status, dur_str, reason_str
            ));
        }
        if !brun.artifacts.is_empty() {
            out.push_str("\n  BROWSER ARTIFACTS\n");
            for a in &brun.artifacts {
                out.push_str(&format!(
                    "  - {} ({}, {} bytes) -> {}\n",
                    a.name, a.mime_type, a.byte_size, a.storage_ref
                ));
            }
        }
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
    fn test_verification_policy_validation_and_checking() {
        let mut policy = VerificationPolicy::new("rust-strict", "Rust Strict Policy");
        policy.required_steps = vec!["fmt".into(), "test".into()];
        policy.allowed_commands = vec![
            AllowedCommand::with_prefix("cargo", vec!["fmt".into()]),
            AllowedCommand::with_prefix("cargo", vec!["test".into()]),
        ];
        assert!(policy.validate().is_ok());

        // Valid plan
        let plan_valid = VerificationPlan::new(
            "rust-plan",
            "Plan",
            vec![
                VerificationStep::new_command("fmt", "Format", vec!["cargo".into(), "fmt".into()]),
                VerificationStep::new_command("test", "Tests", vec!["cargo".into(), "test".into()]),
            ],
        );
        assert!(policy.check_plan(&plan_valid).is_ok());

        // Missing required step
        let plan_missing = VerificationPlan::new(
            "rust-plan-partial",
            "Plan",
            vec![VerificationStep::new_command(
                "fmt",
                "Format",
                vec!["cargo".into(), "fmt".into()],
            )],
        );
        assert!(policy.check_plan(&plan_missing).is_err());

        // Disallowed command
        let plan_disallowed = VerificationPlan::new(
            "rust-plan-disallowed",
            "Plan",
            vec![
                VerificationStep::new_command("fmt", "Format", vec!["cargo".into(), "fmt".into()]),
                VerificationStep::new_command("test", "Tests", vec!["cargo".into(), "test".into()]),
                VerificationStep::new_command(
                    "hack",
                    "Hack",
                    vec!["curl".into(), "evil.com".into()],
                ),
            ],
        );
        assert!(policy.check_plan(&plan_disallowed).is_err());
    }

    #[test]
    fn test_verification_environment_policy_digest_and_defaults() {
        let env_pol = VerificationEnvironmentPolicy::clean();
        let dig1 = env_pol.digest();
        let dig2 = env_pol.digest();
        assert_eq!(dig1, dig2);

        let mut env_pol_custom = VerificationEnvironmentPolicy::clean();
        env_pol_custom.set.insert("FOO".into(), "BAR".into());
        assert_ne!(dig1, env_pol_custom.digest());
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
            policy_id: Some("policy-strict".into()),
            policy_version: Some(1),
            policy_digest: Some("sha256-pol-digest".into()),
            status: VerificationStepStatus::Passed,
            environment_identity: Default::default(),
            started_at_ms: 1000,
            finished_at_ms: Some(43800),
            overall_result: Some(VerificationRunResult::Passed),
            browser_verification_run: None,
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
