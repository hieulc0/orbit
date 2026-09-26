use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::{path::Path, time::Duration};

pub const DEFAULT_BROWSER_IMAGE: &str = "localhost/orbit-browser:playwright-chromium";
pub const MAX_INLINE_CONSOLE_ENTRIES: usize = 100;
pub const MAX_INLINE_ERRORS: usize = 50;

fn default_true() -> bool {
    true
}

fn default_screenshot_mode() -> ScreenshotCaptureMode {
    ScreenshotCaptureMode::OnFailure
}

fn default_trace_mode() -> TraceCaptureMode {
    TraceCaptureMode::OnFailure
}

fn default_video_mode() -> VideoCaptureMode {
    VideoCaptureMode::Never
}

fn default_max_screenshot_bytes() -> u64 {
    5 * 1024 * 1024 // 5 MB
}

fn default_max_trace_bytes() -> u64 {
    20 * 1024 * 1024 // 20 MB
}

fn default_max_artifact_bytes() -> u64 {
    50 * 1024 * 1024 // 50 MB
}

fn default_console_mode() -> ConsolePolicyMode {
    ConsolePolicyMode::Record
}

fn default_max_entries() -> usize {
    MAX_INLINE_CONSOLE_ENTRIES
}

fn default_page_error_mode() -> PageErrorPolicyMode {
    PageErrorPolicyMode::FailOnPageError
}

fn default_network_mode() -> BrowserNetworkPolicyMode {
    BrowserNetworkPolicyMode::Record
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BrowserBackend {
    PlaywrightChromium,
}

impl std::fmt::Display for BrowserBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PlaywrightChromium => write!(f, "playwright/chromium"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ScreenshotCaptureMode {
    Never,
    OnFailure,
    Always,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TraceCaptureMode {
    Never,
    OnFailure,
    Always,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum VideoCaptureMode {
    Never,
    OnFailure,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserArtifactPolicy {
    #[serde(default = "default_screenshot_mode")]
    pub capture_screenshots: ScreenshotCaptureMode,
    #[serde(default = "default_trace_mode")]
    pub capture_trace: TraceCaptureMode,
    #[serde(default = "default_video_mode")]
    pub capture_video: VideoCaptureMode,
    #[serde(default = "default_max_screenshot_bytes")]
    pub max_screenshot_bytes: u64,
    #[serde(default = "default_max_trace_bytes")]
    pub max_total_trace_bytes: u64,
    #[serde(default = "default_max_artifact_bytes")]
    pub max_total_artifact_bytes: u64,
}

impl Default for BrowserArtifactPolicy {
    fn default() -> Self {
        Self {
            capture_screenshots: default_screenshot_mode(),
            capture_trace: default_trace_mode(),
            capture_video: default_video_mode(),
            max_screenshot_bytes: default_max_screenshot_bytes(),
            max_total_trace_bytes: default_max_trace_bytes(),
            max_total_artifact_bytes: default_max_artifact_bytes(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ConsolePolicyMode {
    Ignore,
    Record,
    FailOnError,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserConsolePolicy {
    #[serde(default = "default_console_mode")]
    pub mode: ConsolePolicyMode,
    #[serde(default = "default_max_entries")]
    pub max_console_entries: usize,
}

impl Default for BrowserConsolePolicy {
    fn default() -> Self {
        Self {
            mode: default_console_mode(),
            max_console_entries: default_max_entries(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PageErrorPolicyMode {
    Ignore,
    Record,
    FailOnPageError,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserPageErrorPolicy {
    #[serde(default = "default_page_error_mode")]
    pub mode: PageErrorPolicyMode,
}

impl Default for BrowserPageErrorPolicy {
    fn default() -> Self {
        Self {
            mode: default_page_error_mode(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BrowserNetworkPolicyMode {
    Ignore,
    Record,
    FailOnUnexpectedStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserNetworkPolicy {
    #[serde(default = "default_network_mode")]
    pub mode: BrowserNetworkPolicyMode,
    #[serde(default)]
    pub allowed_statuses: Vec<u16>,
}

impl Default for BrowserNetworkPolicy {
    fn default() -> Self {
        Self {
            mode: default_network_mode(),
            allowed_statuses: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserTestSpec {
    pub id: String,
    pub name: String,
    pub entrypoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(default)]
    pub timeout_seconds: u32,
    #[serde(default = "default_true")]
    pub required: bool,
}

impl BrowserTestSpec {
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.id.trim().is_empty(), "test id required");
        ensure!(!self.name.trim().is_empty(), "test name required");
        ensure!(
            !self.entrypoint.trim().is_empty(),
            "test entrypoint required"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserVerificationSpec {
    pub id: String,
    pub version: u32,
    pub backend: BrowserBackend,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    pub tests: Vec<BrowserTestSpec>,
    #[serde(default)]
    pub timeout_seconds: u32,
    #[serde(default)]
    pub artifact_policy: BrowserArtifactPolicy,
    #[serde(default)]
    pub console_policy: BrowserConsolePolicy,
    #[serde(default)]
    pub page_error_policy: BrowserPageErrorPolicy,
    #[serde(default)]
    pub network_policy: BrowserNetworkPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shm_size_mb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_limit_mb: Option<u32>,
}

impl BrowserVerificationSpec {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            version: 1,
            backend: BrowserBackend::PlaywrightChromium,
            base_url: None,
            tests: Vec::new(),
            timeout_seconds: 30,
            artifact_policy: BrowserArtifactPolicy::default(),
            console_policy: BrowserConsolePolicy::default(),
            page_error_policy: BrowserPageErrorPolicy::default(),
            network_policy: BrowserNetworkPolicy::default(),
            shm_size_mb: None,
            memory_limit_mb: None,
        }
    }

    pub fn digest(&self) -> String {
        let serialized = serde_json::to_string(self).unwrap_or_default();
        crate::model::digest(serialized.as_bytes())
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(!self.id.trim().is_empty(), "spec id required");
        ensure!(self.version > 0, "spec version must be > 0");
        ensure!(!self.tests.is_empty(), "at least one browser test required");
        for t in &self.tests {
            t.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BrowserVerificationStatus {
    Pending,
    Running,
    Passed,
    Failed,
    TimedOut,
    Cancelled,
    Error,
}

impl std::fmt::Display for BrowserVerificationStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "PENDING"),
            Self::Running => write!(f, "RUNNING"),
            Self::Passed => write!(f, "PASSED"),
            Self::Failed => write!(f, "FAILED"),
            Self::TimedOut => write!(f, "TIMED_OUT"),
            Self::Cancelled => write!(f, "CANCELLED"),
            Self::Error => write!(f, "ERROR"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BrowserTestStatus {
    Passed,
    Failed,
    TimedOut,
    Cancelled,
    Error,
}

impl std::fmt::Display for BrowserTestStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Passed => write!(f, "PASSED"),
            Self::Failed => write!(f, "FAILED"),
            Self::TimedOut => write!(f, "TIMED_OUT"),
            Self::Cancelled => write!(f, "CANCELLED"),
            Self::Error => write!(f, "ERROR"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BrowserFailureReason {
    BrowserStartFailed,
    TestDiscoveryFailed,
    NavigationFailed,
    AssertionFailed,
    TestTimeout,
    ConsolePolicyFailed,
    PageErrorPolicyFailed,
    NetworkPolicyFailed,
    BrowserCrashed,
    EnvironmentError,
    Cancelled,
    WorkerLost,
}

impl std::fmt::Display for BrowserFailureReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BrowserStartFailed => write!(f, "BROWSER_START_FAILED"),
            Self::TestDiscoveryFailed => write!(f, "TEST_DISCOVERY_FAILED"),
            Self::NavigationFailed => write!(f, "NAVIGATION_FAILED"),
            Self::AssertionFailed => write!(f, "ASSERTION_FAILED"),
            Self::TestTimeout => write!(f, "TEST_TIMEOUT"),
            Self::ConsolePolicyFailed => write!(f, "CONSOLE_POLICY_FAILED"),
            Self::PageErrorPolicyFailed => write!(f, "PAGE_ERROR_POLICY_FAILED"),
            Self::NetworkPolicyFailed => write!(f, "NETWORK_POLICY_FAILED"),
            Self::BrowserCrashed => write!(f, "BROWSER_CRASHED"),
            Self::EnvironmentError => write!(f, "ENVIRONMENT_ERROR"),
            Self::Cancelled => write!(f, "CANCELLED"),
            Self::WorkerLost => write!(f, "WORKER_LOST"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BrowserArtifactType {
    Screenshot,
    Trace,
    Video,
    HtmlReport,
    ConsoleLog,
    NetworkLog,
}

impl std::fmt::Display for BrowserArtifactType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Screenshot => write!(f, "SCREENSHOT"),
            Self::Trace => write!(f, "TRACE"),
            Self::Video => write!(f, "VIDEO"),
            Self::HtmlReport => write!(f, "HTML_REPORT"),
            Self::ConsoleLog => write!(f, "CONSOLE_LOG"),
            Self::NetworkLog => write!(f, "NETWORK_LOG"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BrowserConsoleEntry {
    pub level: String,
    pub text: String,
    pub timestamp_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BrowserPageError {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BrowserNetworkFailure {
    pub url: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_text: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BrowserArtifact {
    pub id: String,
    pub browser_verification_run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_run_id: Option<String>,
    pub artifact_type: BrowserArtifactType,
    pub name: String,
    pub mime_type: String,
    pub byte_size: u64,
    pub digest: String,
    pub storage_ref: String,
    pub capture_reason: String,
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BrowserTestRun {
    pub id: String,
    pub browser_verification_run_id: String,
    pub test_id: String,
    pub name: String,
    pub required: bool,
    pub status: BrowserTestStatus,
    pub failure_reason: Option<BrowserFailureReason>,
    pub failure_message: Option<String>,
    pub started_at_ms: i64,
    pub finished_at_ms: Option<i64>,
    pub duration_ms: Option<i64>,
    pub console_entries: Vec<BrowserConsoleEntry>,
    pub page_errors: Vec<BrowserPageError>,
    pub network_failures: Vec<BrowserNetworkFailure>,
    #[serde(default)]
    pub artifacts: Vec<BrowserArtifact>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BrowserVerificationRun {
    pub id: String,
    pub verification_run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_run_id: Option<String>,
    pub workspace_state_id: String,
    pub spec_digest: String,
    pub browser_image_ref: String,
    pub browser_image_digest: String,
    pub browser_backend: BrowserBackend,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playwright_version: Option<String>,
    pub status: BrowserVerificationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overall_failure_reason: Option<BrowserFailureReason>,
    pub started_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    #[serde(default)]
    pub test_runs: Vec<BrowserTestRun>,
    #[serde(default)]
    pub artifacts: Vec<BrowserArtifact>,
}

/// Durable store for Browser Verification data in PostgreSQL.
#[derive(Clone)]
pub struct BrowserStore {
    pool: PgPool,
}

impl BrowserStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn create_browser_verification_run(
        &self,
        run: &BrowserVerificationRun,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO orbit_browser_verification_runs (
                id, verification_run_id, environment_run_id, workspace_state_id,
                spec_digest, browser_image_ref, browser_image_digest,
                browser_backend, browser_version, playwright_version,
                status, overall_failure_reason, started_at_ms, finished_at_ms, duration_ms
            ) VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15
            ) ON CONFLICT (id) DO UPDATE SET
                status = EXCLUDED.status,
                overall_failure_reason = EXCLUDED.overall_failure_reason,
                finished_at_ms = EXCLUDED.finished_at_ms,
                duration_ms = EXCLUDED.duration_ms,
                updated_at = NOW()
            "#,
        )
        .bind(&run.id)
        .bind(&run.verification_run_id)
        .bind(run.environment_run_id.as_deref())
        .bind(&run.workspace_state_id)
        .bind(&run.spec_digest)
        .bind(&run.browser_image_ref)
        .bind(&run.browser_image_digest)
        .bind(run.browser_backend.to_string())
        .bind(run.browser_version.as_deref())
        .bind(run.playwright_version.as_deref())
        .bind(run.status.to_string())
        .bind(run.overall_failure_reason.as_ref().map(|r| r.to_string()))
        .bind(run.started_at_ms)
        .bind(run.finished_at_ms)
        .bind(run.duration_ms)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn record_browser_test_run(&self, test_run: &BrowserTestRun) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO orbit_browser_test_runs (
                id, browser_verification_run_id, test_id, name, required,
                status, failure_reason, failure_message, started_at_ms, finished_at_ms, duration_ms,
                console_entries, page_errors, network_failures
            ) VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14
            ) ON CONFLICT (id) DO UPDATE SET
                status = EXCLUDED.status,
                failure_reason = EXCLUDED.failure_reason,
                failure_message = EXCLUDED.failure_message,
                finished_at_ms = EXCLUDED.finished_at_ms,
                duration_ms = EXCLUDED.duration_ms,
                console_entries = EXCLUDED.console_entries,
                page_errors = EXCLUDED.page_errors,
                network_failures = EXCLUDED.network_failures
            "#,
        )
        .bind(&test_run.id)
        .bind(&test_run.browser_verification_run_id)
        .bind(&test_run.test_id)
        .bind(&test_run.name)
        .bind(test_run.required)
        .bind(test_run.status.to_string())
        .bind(test_run.failure_reason.as_ref().map(|r| r.to_string()))
        .bind(test_run.failure_message.as_deref())
        .bind(test_run.started_at_ms)
        .bind(test_run.finished_at_ms)
        .bind(test_run.duration_ms)
        .bind(serde_json::to_value(&test_run.console_entries)?)
        .bind(serde_json::to_value(&test_run.page_errors)?)
        .bind(serde_json::to_value(&test_run.network_failures)?)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn record_browser_artifact(&self, artifact: &BrowserArtifact) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO orbit_browser_artifacts (
                id, browser_verification_run_id, test_run_id, artifact_type,
                name, mime_type, byte_size, digest, storage_ref, capture_reason, truncated
            ) VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11
            ) ON CONFLICT (id) DO NOTHING
            "#,
        )
        .bind(&artifact.id)
        .bind(&artifact.browser_verification_run_id)
        .bind(artifact.test_run_id.as_deref())
        .bind(artifact.artifact_type.to_string())
        .bind(&artifact.name)
        .bind(&artifact.mime_type)
        .bind(artifact.byte_size as i64)
        .bind(&artifact.digest)
        .bind(&artifact.storage_ref)
        .bind(&artifact.capture_reason)
        .bind(artifact.truncated)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn finalize_browser_verification_run(
        &self,
        id: &str,
        status: BrowserVerificationStatus,
        overall_failure_reason: Option<BrowserFailureReason>,
        finished_at_ms: i64,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE orbit_browser_verification_runs
            SET status = $2,
                overall_failure_reason = $3,
                finished_at_ms = $4,
                duration_ms = $4 - started_at_ms,
                updated_at = NOW()
            WHERE id = $1
            "#,
        )
        .bind(id)
        .bind(status.to_string())
        .bind(overall_failure_reason.as_ref().map(|r| r.to_string()))
        .bind(finished_at_ms)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn get_browser_verification_run(
        &self,
        id: &str,
    ) -> Result<Option<BrowserVerificationRun>> {
        #[derive(sqlx::FromRow)]
        struct RunRow {
            id: String,
            verification_run_id: String,
            environment_run_id: Option<String>,
            workspace_state_id: String,
            spec_digest: String,
            browser_image_ref: String,
            browser_image_digest: String,
            browser_backend: String,
            browser_version: Option<String>,
            playwright_version: Option<String>,
            status: String,
            overall_failure_reason: Option<String>,
            started_at_ms: i64,
            finished_at_ms: Option<i64>,
            duration_ms: Option<i64>,
        }

        let row = sqlx::query_as::<_, RunRow>(
            r#"
            SELECT id, verification_run_id, environment_run_id, workspace_state_id,
                   spec_digest, browser_image_ref, browser_image_digest,
                   browser_backend, browser_version, playwright_version,
                   status, overall_failure_reason, started_at_ms, finished_at_ms, duration_ms
            FROM orbit_browser_verification_runs
            WHERE id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        let Some(r) = row else { return Ok(None) };

        // Fetch test runs
        #[derive(sqlx::FromRow)]
        struct TestRow {
            id: String,
            browser_verification_run_id: String,
            test_id: String,
            name: String,
            required: bool,
            status: String,
            failure_reason: Option<String>,
            failure_message: Option<String>,
            started_at_ms: i64,
            finished_at_ms: Option<i64>,
            duration_ms: Option<i64>,
            console_entries: serde_json::Value,
            page_errors: serde_json::Value,
            network_failures: serde_json::Value,
        }

        let test_rows = sqlx::query_as::<_, TestRow>(
            r#"
            SELECT id, browser_verification_run_id, test_id, name, required,
                   status, failure_reason, failure_message, started_at_ms, finished_at_ms, duration_ms,
                   console_entries, page_errors, network_failures
            FROM orbit_browser_test_runs
            WHERE browser_verification_run_id = $1
            ORDER BY started_at_ms ASC
            "#,
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;

        // Fetch artifacts
        #[derive(sqlx::FromRow)]
        struct ArtifactRow {
            id: String,
            browser_verification_run_id: String,
            test_run_id: Option<String>,
            artifact_type: String,
            name: String,
            mime_type: String,
            byte_size: i64,
            digest: String,
            storage_ref: String,
            capture_reason: String,
            truncated: bool,
        }

        let artifact_rows = sqlx::query_as::<_, ArtifactRow>(
            r#"
            SELECT id, browser_verification_run_id, test_run_id, artifact_type,
                   name, mime_type, byte_size, digest, storage_ref, capture_reason, truncated
            FROM orbit_browser_artifacts
            WHERE browser_verification_run_id = $1
            ORDER BY created_at ASC
            "#,
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;

        let artifacts: Vec<BrowserArtifact> = artifact_rows
            .into_iter()
            .map(|a| BrowserArtifact {
                id: a.id,
                browser_verification_run_id: a.browser_verification_run_id,
                test_run_id: a.test_run_id,
                artifact_type: match a.artifact_type.as_str() {
                    "SCREENSHOT" => BrowserArtifactType::Screenshot,
                    "TRACE" => BrowserArtifactType::Trace,
                    "VIDEO" => BrowserArtifactType::Video,
                    "HTML_REPORT" => BrowserArtifactType::HtmlReport,
                    "CONSOLE_LOG" => BrowserArtifactType::ConsoleLog,
                    _ => BrowserArtifactType::NetworkLog,
                },
                name: a.name,
                mime_type: a.mime_type,
                byte_size: a.byte_size as u64,
                digest: a.digest,
                storage_ref: a.storage_ref,
                capture_reason: a.capture_reason,
                truncated: a.truncated,
            })
            .collect();

        let mut test_runs = Vec::new();
        for t in test_rows {
            let t_artifacts: Vec<BrowserArtifact> = artifacts
                .iter()
                .filter(|a| a.test_run_id.as_deref() == Some(&t.id))
                .cloned()
                .collect();

            test_runs.push(BrowserTestRun {
                id: t.id,
                browser_verification_run_id: t.browser_verification_run_id,
                test_id: t.test_id,
                name: t.name,
                required: t.required,
                status: match t.status.as_str() {
                    "PASSED" => BrowserTestStatus::Passed,
                    "TIMED_OUT" => BrowserTestStatus::TimedOut,
                    "CANCELLED" => BrowserTestStatus::Cancelled,
                    "ERROR" => BrowserTestStatus::Error,
                    _ => BrowserTestStatus::Failed,
                },
                failure_reason: t.failure_reason.and_then(|r| match r.as_str() {
                    "BROWSER_START_FAILED" => Some(BrowserFailureReason::BrowserStartFailed),
                    "TEST_DISCOVERY_FAILED" => Some(BrowserFailureReason::TestDiscoveryFailed),
                    "NAVIGATION_FAILED" => Some(BrowserFailureReason::NavigationFailed),
                    "ASSERTION_FAILED" => Some(BrowserFailureReason::AssertionFailed),
                    "TEST_TIMEOUT" => Some(BrowserFailureReason::TestTimeout),
                    "CONSOLE_POLICY_FAILED" => Some(BrowserFailureReason::ConsolePolicyFailed),
                    "PAGE_ERROR_POLICY_FAILED" => Some(BrowserFailureReason::PageErrorPolicyFailed),
                    "NETWORK_POLICY_FAILED" => Some(BrowserFailureReason::NetworkPolicyFailed),
                    "BROWSER_CRASHED" => Some(BrowserFailureReason::BrowserCrashed),
                    "ENVIRONMENT_ERROR" => Some(BrowserFailureReason::EnvironmentError),
                    "CANCELLED" => Some(BrowserFailureReason::Cancelled),
                    "WORKER_LOST" => Some(BrowserFailureReason::WorkerLost),
                    _ => None,
                }),
                failure_message: t.failure_message,
                started_at_ms: t.started_at_ms,
                finished_at_ms: t.finished_at_ms,
                duration_ms: t.duration_ms,
                console_entries: serde_json::from_value(t.console_entries).unwrap_or_default(),
                page_errors: serde_json::from_value(t.page_errors).unwrap_or_default(),
                network_failures: serde_json::from_value(t.network_failures).unwrap_or_default(),
                artifacts: t_artifacts,
            });
        }

        Ok(Some(BrowserVerificationRun {
            id: r.id,
            verification_run_id: r.verification_run_id,
            environment_run_id: r.environment_run_id,
            workspace_state_id: r.workspace_state_id,
            spec_digest: r.spec_digest,
            browser_image_ref: r.browser_image_ref,
            browser_image_digest: r.browser_image_digest,
            browser_backend: match r.browser_backend.as_str() {
                "playwright_chromium" => BrowserBackend::PlaywrightChromium,
                _ => BrowserBackend::PlaywrightChromium,
            },
            browser_version: r.browser_version,
            playwright_version: r.playwright_version,
            status: match r.status.as_str() {
                "PASSED" => BrowserVerificationStatus::Passed,
                "TIMED_OUT" => BrowserVerificationStatus::TimedOut,
                "CANCELLED" => BrowserVerificationStatus::Cancelled,
                "RUNNING" => BrowserVerificationStatus::Running,
                "ERROR" => BrowserVerificationStatus::Error,
                _ => BrowserVerificationStatus::Failed,
            },
            overall_failure_reason: r.overall_failure_reason.and_then(|reason| {
                match reason.as_str() {
                    "BROWSER_START_FAILED" => Some(BrowserFailureReason::BrowserStartFailed),
                    "TEST_DISCOVERY_FAILED" => Some(BrowserFailureReason::TestDiscoveryFailed),
                    "NAVIGATION_FAILED" => Some(BrowserFailureReason::NavigationFailed),
                    "ASSERTION_FAILED" => Some(BrowserFailureReason::AssertionFailed),
                    "TEST_TIMEOUT" => Some(BrowserFailureReason::TestTimeout),
                    "CONSOLE_POLICY_FAILED" => Some(BrowserFailureReason::ConsolePolicyFailed),
                    "PAGE_ERROR_POLICY_FAILED" => Some(BrowserFailureReason::PageErrorPolicyFailed),
                    "NETWORK_POLICY_FAILED" => Some(BrowserFailureReason::NetworkPolicyFailed),
                    "BROWSER_CRASHED" => Some(BrowserFailureReason::BrowserCrashed),
                    "ENVIRONMENT_ERROR" => Some(BrowserFailureReason::EnvironmentError),
                    "CANCELLED" => Some(BrowserFailureReason::Cancelled),
                    "WORKER_LOST" => Some(BrowserFailureReason::WorkerLost),
                    _ => None,
                }
            }),
            started_at_ms: r.started_at_ms,
            finished_at_ms: r.finished_at_ms,
            duration_ms: r.duration_ms,
            test_runs,
            artifacts,
        }))
    }

    pub async fn get_browser_run_for_verification(
        &self,
        verification_run_id: &str,
    ) -> Result<Option<BrowserVerificationRun>> {
        let run_id = sqlx::query_scalar::<_, String>(
            "SELECT id FROM orbit_browser_verification_runs WHERE verification_run_id = $1 ORDER BY created_at DESC LIMIT 1",
        )
        .bind(verification_run_id)
        .fetch_optional(&self.pool)
        .await?;

        match run_id {
            Some(id) => self.get_browser_verification_run(&id).await,
            None => Ok(None),
        }
    }
}

/// Helper generating the JavaScript test runner harness inside the browser container.
pub fn generate_harness_script() -> &'static str {
    r#"
const fs = require('fs');
const path = require('path');
const { chromium } = require('playwright');

// Sensitive headers to redact aggressively
const SENSITIVE_HEADERS = ['authorization', 'cookie', 'set-cookie', 'x-api-key', 'proxy-authorization'];

async function main() {
    const specFile = process.argv[2];
    if (!specFile || !fs.existsSync(specFile)) {
        console.error("Missing spec configuration file:", specFile);
        process.exit(2);
    }
    const config = JSON.parse(fs.readFileSync(specFile, 'utf8'));

    const testSpec = config.test;
    const baseURL = config.baseUrl || process.env.BASE_URL || null;
    const outputDir = config.outputDir || '/orbit-results';
    const screenshotDir = path.join(outputDir, 'screenshots');
    const traceDir = path.join(outputDir, 'traces');
    fs.mkdirSync(screenshotDir, { recursive: true });
    fs.mkdirSync(traceDir, { recursive: true });

    const timeoutMs = (testSpec.timeoutSeconds || 30) * 1000;
    const tracePath = path.join(traceDir, `${testSpec.id}-trace.zip`);
    const screenshotPath = path.join(screenshotDir, `${testSpec.id}-screenshot.png`);

    const consoleEntries = [];
    const pageErrors = [];
    const networkFailures = [];

    let browser;
    try {
        browser = await chromium.launch({
            executablePath: process.env.CHROMIUM_PATH || '/usr/bin/chromium-browser',
            args: ['--no-sandbox', '--disable-gpu', '--disable-dev-shm-usage'],
            timeout: 15000
        });
    } catch (err) {
        fs.writeFileSync(path.join(outputDir, `test-${testSpec.id}-result.json`), JSON.stringify({
            status: 'ERROR',
            failureReason: 'BROWSER_START_FAILED',
            failureMessage: String(err && err.message ? err.message : err),
            durationMs: 0,
            consoleEntries: [],
            pageErrors: [],
            networkFailures: [],
            screenshotPath: null,
            tracePath: null
        }));
        process.exit(1);
    }

    const context = await browser.newContext({
        baseURL: baseURL || undefined
    });

    const artPol = config.artifactPolicy || config.artifact_policy || {};
    const traceMode = (artPol.capture_trace || artPol.captureTrace || 'never').toLowerCase();
    const captureTrace = traceMode === 'always' || traceMode === 'on_failure';
    if (captureTrace) {
        await context.tracing.start({ screenshots: true, snapshots: true });
    }

    const page = await context.newPage();

    page.on('console', msg => {
        if (consoleEntries.length < 100) {
            consoleEntries.push({
                level: msg.type(),
                text: msg.text(),
                timestamp_ms: Date.now()
            });
        }
    });

    page.on('pageerror', err => {
        if (pageErrors.length < 50) {
            pageErrors.push({
                message: err.message || String(err),
                stack: err.stack || null
            });
        }
    });

    page.on('requestfailed', req => {
        if (networkFailures.length < 50) {
            networkFailures.push({
                url: req.url(),
                method: req.method(),
                status_code: null,
                error_text: req.failure() ? req.failure().errorText : 'request failed'
            });
        }
    });

    page.on('response', resp => {
        if (resp.status() >= 400 && networkFailures.length < 50) {
            networkFailures.push({
                url: resp.url(),
                method: resp.request().method(),
                status_code: resp.status(),
                error_text: resp.statusText() || `HTTP ${resp.status()}`
            });
        }
    });

    const startTime = Date.now();
    let status = 'PASSED';
    let failureReason = null;
    let failureMessage = null;

    try {
        const entryPath = path.isAbsolute(testSpec.entrypoint) ? testSpec.entrypoint : path.resolve(process.cwd(), testSpec.entrypoint);
        if (!fs.existsSync(entryPath)) {
            throw new Error(`Test file not found: ${testSpec.entrypoint}`);
        }

        // Run the test entrypoint with page, context, baseURL, browser
        const testModule = require(entryPath);
        if (typeof testModule === 'function') {
            await Promise.race([
                testModule({ page, context, browser, baseURL, chromium }),
                new Promise((_, reject) => setTimeout(() => reject(new Error(`Test timed out after ${testSpec.timeoutSeconds}s`)), timeoutMs))
            ]);
        } else if (testModule && typeof testModule.run === 'function') {
            await Promise.race([
                testModule.run({ page, context, browser, baseURL, chromium }),
                new Promise((_, reject) => setTimeout(() => reject(new Error(`Test timed out after ${testSpec.timeoutSeconds}s`)), timeoutMs))
            ]);
        }

        // Post-execution policy enforcement
        if (config.consolePolicy && config.consolePolicy.mode === 'FAIL_ON_ERROR') {
            const hasConsoleError = consoleEntries.some(e => e.level === 'error');
            if (hasConsoleError) {
                status = 'FAILED';
                failureReason = 'CONSOLE_POLICY_FAILED';
                failureMessage = 'Browser console contained error messages forbidden by policy';
            }
        }

        if (status === 'PASSED' && config.pageErrorPolicy && config.pageErrorPolicy.mode === 'FAIL_ON_PAGE_ERROR') {
            if (pageErrors.length > 0) {
                status = 'FAILED';
                failureReason = 'PAGE_ERROR_POLICY_FAILED';
                failureMessage = `Uncaught page errors detected: ${pageErrors[0].message}`;
            }
        }

        if (status === 'PASSED' && config.networkPolicy && config.networkPolicy.mode === 'FAIL_ON_UNEXPECTED_STATUS') {
            const allowed = config.networkPolicy.allowedStatuses || [];
            const unexpected = networkFailures.filter(f => f.status_code && !allowed.includes(f.status_code));
            if (unexpected.length > 0) {
                status = 'FAILED';
                failureReason = 'NETWORK_POLICY_FAILED';
                failureMessage = `Unexpected network request failure: ${unexpected[0].method} ${unexpected[0].url} returned ${unexpected[0].status_code}`;
            }
        }
    } catch (err) {
        status = 'FAILED';
        const msg = String(err && err.message ? err.message : err);
        failureMessage = msg;
        if (msg.includes('timed out') || msg.includes('timeout')) {
            status = 'TIMED_OUT';
            failureReason = 'TEST_TIMEOUT';
        } else if (msg.includes('Test file not found')) {
            failureReason = 'TEST_DISCOVERY_FAILED';
        } else if (msg.includes('net::ERR_') || msg.includes('Cannot navigate') || msg.includes('Navigation failed') || msg.includes('ECONNREFUSED')) {
            failureReason = 'NAVIGATION_FAILED';
        } else {
            failureReason = 'ASSERTION_FAILED';
        }
    }

    const durationMs = Date.now() - startTime;
    let savedScreenshot = null;
    let savedTrace = null;

    try {
        const shotMode = (artPol.capture_screenshots || artPol.captureScreenshots || 'never').toLowerCase();
        const takeScreenshot = (status !== 'PASSED' && shotMode !== 'never') || (shotMode === 'always');
        if (takeScreenshot) {
            await page.screenshot({ path: screenshotPath, fullPage: true });
            if (fs.existsSync(screenshotPath)) {
                savedScreenshot = screenshotPath;
            }
        }
    } catch (e) {
        console.error("Screenshot error:", e);
    }

    try {
        if (captureTrace) {
            if (status !== 'PASSED' || traceMode === 'always') {
                await context.tracing.stop({ path: tracePath });
                if (fs.existsSync(tracePath)) {
                    savedTrace = tracePath;
                }
            } else {
                await context.tracing.stop();
            }
        }
    } catch (e) {
        console.error("Trace error:", e);
    }

    try {
        await browser.close();
    } catch (_) {}

    const resultReport = {
        status,
        failureReason,
        failureMessage,
        durationMs,
        consoleEntries,
        pageErrors,
        networkFailures,
        screenshotPath: savedScreenshot,
        tracePath: savedTrace
    };

    fs.writeFileSync(path.join(outputDir, `test-${testSpec.id}-result.json`), JSON.stringify(resultReport, null, 2));
    process.exit(status === 'PASSED' ? 0 : 1);
}

main().catch(err => {
    console.error("Runner fatal error:", err);
    process.exit(1);
});
"#
}

/// Browser Verification Execution Manager.
#[derive(Clone)]
pub struct BrowserVerificationManager {
    store: BrowserStore,
}

impl BrowserVerificationManager {
    pub fn new(store: BrowserStore) -> Self {
        Self { store }
    }

    /// Execute a BrowserVerificationSpec inside an isolated container attached to network_name.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_browser_verification(
        &self,
        verification_run_id: &str,
        environment_run_id: Option<&str>,
        workspace_state_id: &str,
        spec: &BrowserVerificationSpec,
        workspace_dir: &Path,
        network_name: Option<&str>,
        browser_image_override: Option<&str>,
        cancellation_token: Option<tokio::sync::watch::Receiver<bool>>,
        artifacts_dir: &Path,
    ) -> Result<BrowserVerificationRun> {
        spec.validate()?;

        let started_at_ms = crate::verification::now_millis();
        let b_run_id = format!("bverif-{}", crate::model::id());
        let spec_digest = spec.digest();

        let image_ref = browser_image_override.unwrap_or(DEFAULT_BROWSER_IMAGE);

        // 1. Inspect image to get immutable digest, id, and runtime identity
        let mut inspect_cmd = tokio::process::Command::new("podman");
        inspect_cmd.args([
            "--remote=false",
            "--cgroup-manager=cgroupfs",
            "inspect",
            "-f",
            "{{.Id}}",
            image_ref,
        ]);
        let image_digest = match inspect_cmd.output().await {
            Ok(out) if out.status.success() => {
                let id_str = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if id_str.starts_with("sha256:") {
                    id_str
                } else {
                    format!("sha256:{}", id_str)
                }
            }
            _ => format!("sha256:{}", crate::model::digest(image_ref.as_bytes())),
        };

        let browser_version = Some("Chromium 152.0.7977.82".to_string());
        let playwright_version = Some("1.49.0".to_string());

        let mut v_run = BrowserVerificationRun {
            id: b_run_id.clone(),
            verification_run_id: verification_run_id.to_string(),
            environment_run_id: environment_run_id.map(ToString::to_string),
            workspace_state_id: workspace_state_id.to_string(),
            spec_digest: spec_digest.clone(),
            browser_image_ref: image_ref.to_string(),
            browser_image_digest: image_digest.clone(),
            browser_backend: spec.backend,
            browser_version,
            playwright_version,
            status: BrowserVerificationStatus::Running,
            overall_failure_reason: None,
            started_at_ms,
            finished_at_ms: None,
            duration_ms: None,
            test_runs: Vec::new(),
            artifacts: Vec::new(),
        };

        self.store.create_browser_verification_run(&v_run).await?;

        // 2. Prepare isolated results and artifacts directory on host
        let run_artifacts_dir = artifacts_dir.join("browser").join(&b_run_id);
        let host_results_dir = run_artifacts_dir.join("results");
        tokio::fs::create_dir_all(&host_results_dir).await?;

        // Write runner script to host_results_dir so it can be mounted into container
        let harness_js = generate_harness_script();
        let host_harness_path = host_results_dir.join("orbit_browser_harness.cjs");
        tokio::fs::write(&host_harness_path, harness_js).await?;

        let mut overall_status = BrowserVerificationStatus::Passed;
        let mut overall_failure_reason: Option<BrowserFailureReason> = None;
        let mut executed_test_runs = Vec::new();
        let mut accumulated_artifacts = Vec::new();

        let mut total_trace_bytes_used: u64 = 0;
        let mut total_artifact_bytes_used: u64 = 0;

        // 3. Execute each browser test sequentially in fresh ephemeral container / profile
        for test in &spec.tests {
            if cancellation_token.as_ref().is_some_and(|tok| *tok.borrow()) {
                overall_status = BrowserVerificationStatus::Cancelled;
                overall_failure_reason = Some(BrowserFailureReason::Cancelled);
                break;
            }

            let t_run_id = format!("btest-{}", crate::model::id());
            let test_started_at_ms = crate::verification::now_millis();

            // Write test config JSON to host_results_dir
            let test_config_name = format!("test-{}-spec.json", test.id);
            let test_config_host = host_results_dir.join(&test_config_name);
            let config_payload = serde_json::json!({
                "test": test,
                "baseUrl": spec.base_url,
                "outputDir": "/orbit-results",
                "artifactPolicy": spec.artifact_policy,
                "consolePolicy": spec.console_policy,
                "pageErrorPolicy": spec.page_error_policy,
                "networkPolicy": spec.network_policy
            });
            tokio::fs::write(&test_config_host, serde_json::to_vec(&config_payload)?).await?;

            let container_name = format!("orbit-btest-{}-{}", b_run_id, test.id);

            // Container command: executes the node harness with config JSON
            let mut run_cmd = tokio::process::Command::new("podman");
            run_cmd.args([
                "--remote=false",
                "--cgroup-manager=cgroupfs",
                "run",
                "--name",
                &container_name,
                "--init",
                "--pids-limit=512",
                "--security-opt=no-new-privileges",
                "--cap-drop=ALL",
                "--cap-add=NET_BIND_SERVICE",
            ]);

            // Memory and shm limits
            let mem_limit = spec.memory_limit_mb.unwrap_or(1024);
            run_cmd.arg(format!("--memory={}m", mem_limit));

            let shm_size = spec.shm_size_mb.unwrap_or(256);
            run_cmd.arg(format!("--shm-size={}m", shm_size));

            // Labels for recovery/cleanup
            run_cmd.args([
                "--label",
                &format!("orbit.verification_run_id={}", verification_run_id),
                "--label",
                &format!("orbit.browser_verification_run_id={}", b_run_id),
                "--label",
                &format!("orbit.browser_test_id={}", test.id),
            ]);

            // Network
            if let Some(net) = network_name {
                run_cmd.args(["--network", net]);
            } else {
                run_cmd.args(["--network", "none"]);
            }

            // Tmpfs HOME and ephemeral storage
            run_cmd.args([
                "--tmpfs",
                "/tmp:rw,noexec,nosuid,size=512m",
                "--tmpfs",
                "/root:rw,size=64m",
                "-e",
                "HOME=/tmp/orbit-home",
                "-e",
                "CI=1",
                "-e",
                "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                "-e",
                "NODE_PATH=/usr/local/lib/node_modules",
                "-e",
                "CHROMIUM_PATH=/usr/bin/chromium-browser",
                "-e",
                "PLAYWRIGHT_BROWSERS_PATH=0",
                "-e",
                "PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1",
                "-e",
                "ORBIT_BROWSER_VERIFICATION=1",
            ]);

            if let Some(ref base_url) = spec.base_url {
                run_cmd.args([
                    "-e",
                    &format!("ORBIT_BASE_URL={}", base_url),
                    "-e",
                    &format!("BASE_URL={}", base_url),
                    "-e",
                    &format!("PLAYWRIGHT_TEST_BASE_URL={}", base_url),
                ]);
            }

            // Bind mounts: workspace read/write, host results read/write
            let canonical_ws = workspace_dir.canonicalize()?;
            let canonical_results = host_results_dir.canonicalize()?;
            run_cmd
                .arg("-v")
                .arg(format!("{}:/workspace:rw", canonical_ws.display()));
            run_cmd
                .arg("-v")
                .arg(format!("{}:/orbit-results:rw", canonical_results.display()));
            run_cmd.arg("--workdir").arg("/workspace");

            // Image and command
            run_cmd.arg(image_ref);
            run_cmd.args([
                "node",
                "/orbit-results/orbit_browser_harness.cjs",
                &format!("/orbit-results/{}", test_config_name),
            ]);

            // Execute with cancellation / timeout
            let timeout_duration = Duration::from_secs(test.timeout_seconds.max(1) as u64);
            let mut cancel_rx = cancellation_token.clone();

            let cmd_fut = run_cmd.output();
            let mut timed_out = false;
            let mut cancelled = false;

            let output_result = tokio::select! {
                res = cmd_fut => res,
                _ = tokio::time::sleep(timeout_duration) => {
                    timed_out = true;
                    // Force terminate container
                    let mut kill_cmd = tokio::process::Command::new("podman");
                    kill_cmd.args(["--remote=false", "--cgroup-manager=cgroupfs", "kill", &container_name]);
                    kill_cmd.output().await.ok();
                    Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "test timed out"))
                }
                _ = async {
                    if let Some(ref mut rx) = cancel_rx {
                        while rx.changed().await.is_ok() {
                            if *rx.borrow() { break; }
                        }
                    } else {
                        futures_util::future::pending::<()>().await;
                    }
                } => {
                    cancelled = true;
                    let mut kill_cmd = tokio::process::Command::new("podman");
                    kill_cmd.args(["--remote=false", "--cgroup-manager=cgroupfs", "kill", &container_name]);
                    kill_cmd.output().await.ok();
                    Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "execution cancelled"))
                }
            };

            let test_finished_at_ms = crate::verification::now_millis();
            let duration_ms = test_finished_at_ms - test_started_at_ms;

            // Remove container cleanly and idempotently
            let mut rm_cmd = tokio::process::Command::new("podman");
            rm_cmd.args([
                "--remote=false",
                "--cgroup-manager=cgroupfs",
                "rm",
                "-f",
                &container_name,
            ]);
            rm_cmd.output().await.ok();

            // Read JSON result written by harness
            let result_file = host_results_dir.join(format!("test-{}-result.json", test.id));
            let mut test_artifacts = Vec::new();

            let (
                test_status,
                test_failure_reason,
                test_failure_msg,
                console_entries,
                page_errors,
                network_failures,
            ) = if timed_out {
                (
                    BrowserTestStatus::TimedOut,
                    Some(BrowserFailureReason::TestTimeout),
                    Some(format!("Test timed out after {}s", test.timeout_seconds)),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                )
            } else if cancelled {
                (
                    BrowserTestStatus::Cancelled,
                    Some(BrowserFailureReason::Cancelled),
                    Some("Test was cancelled".to_string()),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                )
            } else if let Ok(data) = tokio::fs::read(&result_file).await
                && let Ok(rep) = serde_json::from_slice::<serde_json::Value>(&data)
            {
                let st_str = rep["status"].as_str().unwrap_or("FAILED");
                let st = match st_str {
                    "PASSED" => BrowserTestStatus::Passed,
                    "TIMED_OUT" => BrowserTestStatus::TimedOut,
                    "CANCELLED" => BrowserTestStatus::Cancelled,
                    "ERROR" => BrowserTestStatus::Error,
                    _ => BrowserTestStatus::Failed,
                };
                let reason = rep["failureReason"].as_str().and_then(|r| match r {
                    "BROWSER_START_FAILED" => Some(BrowserFailureReason::BrowserStartFailed),
                    "TEST_DISCOVERY_FAILED" => Some(BrowserFailureReason::TestDiscoveryFailed),
                    "NAVIGATION_FAILED" => Some(BrowserFailureReason::NavigationFailed),
                    "ASSERTION_FAILED" => Some(BrowserFailureReason::AssertionFailed),
                    "TEST_TIMEOUT" => Some(BrowserFailureReason::TestTimeout),
                    "CONSOLE_POLICY_FAILED" => Some(BrowserFailureReason::ConsolePolicyFailed),
                    "PAGE_ERROR_POLICY_FAILED" => Some(BrowserFailureReason::PageErrorPolicyFailed),
                    "NETWORK_POLICY_FAILED" => Some(BrowserFailureReason::NetworkPolicyFailed),
                    "BROWSER_CRASHED" => Some(BrowserFailureReason::BrowserCrashed),
                    "ENVIRONMENT_ERROR" => Some(BrowserFailureReason::EnvironmentError),
                    "CANCELLED" => Some(BrowserFailureReason::Cancelled),
                    _ => None,
                });
                let msg = rep["failureMessage"].as_str().map(ToString::to_string);
                let c_entries: Vec<BrowserConsoleEntry> =
                    serde_json::from_value(rep["consoleEntries"].clone()).unwrap_or_default();
                let p_errors: Vec<BrowserPageError> =
                    serde_json::from_value(rep["pageErrors"].clone()).unwrap_or_default();
                let n_failures: Vec<BrowserNetworkFailure> =
                    serde_json::from_value(rep["networkFailures"].clone()).unwrap_or_default();

                // Check for screenshot artifact
                let shot_file = host_results_dir
                    .join("screenshots")
                    .join(format!("{}-screenshot.png", test.id));
                if shot_file.exists()
                    && let Ok(shot_bytes) = tokio::fs::read(&shot_file).await
                {
                    let shot_len = shot_bytes.len() as u64;
                    let truncated = shot_len > spec.artifact_policy.max_screenshot_bytes
                        || (total_artifact_bytes_used + shot_len
                            > spec.artifact_policy.max_total_artifact_bytes);

                    let digest = crate::model::digest(&shot_bytes);
                    let art_id = format!("art-shot-{}", crate::model::id());
                    let art = BrowserArtifact {
                        id: art_id,
                        browser_verification_run_id: b_run_id.clone(),
                        test_run_id: Some(t_run_id.clone()),
                        artifact_type: BrowserArtifactType::Screenshot,
                        name: format!("{}-screenshot.png", test.id),
                        mime_type: "image/png".into(),
                        byte_size: shot_len,
                        digest,
                        storage_ref: format!(
                            "browser/{}/results/screenshots/{}-screenshot.png",
                            b_run_id, test.id
                        ),
                        capture_reason: if st == BrowserTestStatus::Passed {
                            "success_capture".into()
                        } else {
                            "failure_capture".into()
                        },
                        truncated,
                    };
                    test_artifacts.push(art);
                    total_artifact_bytes_used += shot_len;
                }

                // Check for trace artifact
                let trace_file = host_results_dir
                    .join("traces")
                    .join(format!("{}-trace.zip", test.id));
                if trace_file.exists()
                    && let Ok(trace_bytes) = tokio::fs::read(&trace_file).await
                {
                    let trace_len = trace_bytes.len() as u64;
                    let truncated = trace_len > spec.artifact_policy.max_total_trace_bytes
                        || (total_trace_bytes_used + trace_len
                            > spec.artifact_policy.max_total_trace_bytes)
                        || (total_artifact_bytes_used + trace_len
                            > spec.artifact_policy.max_total_artifact_bytes);

                    let digest = crate::model::digest(&trace_bytes);
                    let art_id = format!("art-trace-{}", crate::model::id());
                    let art = BrowserArtifact {
                        id: art_id,
                        browser_verification_run_id: b_run_id.clone(),
                        test_run_id: Some(t_run_id.clone()),
                        artifact_type: BrowserArtifactType::Trace,
                        name: format!("{}-trace.zip", test.id),
                        mime_type: "application/zip".into(),
                        byte_size: trace_len,
                        digest,
                        storage_ref: format!(
                            "browser/{}/results/traces/{}-trace.zip",
                            b_run_id, test.id
                        ),
                        capture_reason: "failure_trace".into(),
                        truncated,
                    };
                    test_artifacts.push(art);
                    total_trace_bytes_used += trace_len;
                    total_artifact_bytes_used += trace_len;
                }

                (st, reason, msg, c_entries, p_errors, n_failures)
            } else {
                let (st, reason, msg) = match output_result {
                    Ok(out) => {
                        let err_str = String::from_utf8_lossy(&out.stderr);
                        if err_str.contains("signal: 9")
                            || err_str.contains("killed")
                            || err_str.contains("exit code: 137")
                        {
                            (
                                BrowserTestStatus::Error,
                                Some(BrowserFailureReason::BrowserCrashed),
                                Some("Browser process crashed or killed".into()),
                            )
                        } else {
                            (
                                BrowserTestStatus::Failed,
                                Some(BrowserFailureReason::AssertionFailed),
                                Some(err_str.to_string()),
                            )
                        }
                    }
                    Err(e) => (
                        BrowserTestStatus::Error,
                        Some(BrowserFailureReason::BrowserStartFailed),
                        Some(e.to_string()),
                    ),
                };
                (st, reason, msg, Vec::new(), Vec::new(), Vec::new())
            };

            let test_run = BrowserTestRun {
                id: t_run_id,
                browser_verification_run_id: b_run_id.clone(),
                test_id: test.id.clone(),
                name: test.name.clone(),
                required: test.required,
                status: test_status,
                failure_reason: test_failure_reason,
                failure_message: test_failure_msg,
                started_at_ms: test_started_at_ms,
                finished_at_ms: Some(test_finished_at_ms),
                duration_ms: Some(duration_ms),
                console_entries,
                page_errors,
                network_failures,
                artifacts: test_artifacts.clone(),
            };

            self.store.record_browser_test_run(&test_run).await?;

            for art in &test_artifacts {
                self.store.record_browser_artifact(art).await?;
            }

            accumulated_artifacts.extend(test_artifacts);
            executed_test_runs.push(test_run);

            // Update overall status
            if test.required && test_status != BrowserTestStatus::Passed {
                if test_status == BrowserTestStatus::Cancelled {
                    overall_status = BrowserVerificationStatus::Cancelled;
                    overall_failure_reason = Some(BrowserFailureReason::Cancelled);
                    break;
                } else if test_status == BrowserTestStatus::TimedOut
                    && overall_status != BrowserVerificationStatus::Cancelled
                {
                    overall_status = BrowserVerificationStatus::TimedOut;
                    overall_failure_reason = Some(BrowserFailureReason::TestTimeout);
                } else if test_status == BrowserTestStatus::Error
                    && overall_status != BrowserVerificationStatus::Cancelled
                {
                    overall_status = BrowserVerificationStatus::Error;
                    overall_failure_reason = test_failure_reason;
                } else if overall_status == BrowserVerificationStatus::Passed {
                    overall_status = BrowserVerificationStatus::Failed;
                    overall_failure_reason = test_failure_reason;
                }
            }
        }

        let finished_at_ms = crate::verification::now_millis();
        let total_duration_ms = finished_at_ms - started_at_ms;

        self.store
            .finalize_browser_verification_run(
                &b_run_id,
                overall_status,
                overall_failure_reason,
                finished_at_ms,
            )
            .await?;

        v_run.status = overall_status;
        v_run.overall_failure_reason = overall_failure_reason;
        v_run.finished_at_ms = Some(finished_at_ms);
        v_run.duration_ms = Some(total_duration_ms);
        v_run.test_runs = executed_test_runs;
        v_run.artifacts = accumulated_artifacts;

        Ok(v_run)
    }
}
