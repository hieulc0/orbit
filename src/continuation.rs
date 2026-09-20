//! Normalized cross-agent continuation and fallback domain models.
//!
//! Orbit owns the durable execution state. Agents are replaceable execution engines.
//!
//! An Attempt owns its workspace for its entire lifetime.
//! An Attempt may sequence multiple [`AgentExecution`] records across different agents
//! operating on the same workspace.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// Machine-readable agent termination classification.
///
/// Indicates how an agent process/provider call terminated.
/// Note: External test/validation failure does NOT belong here; validation is
/// performed by Orbit after agent termination and is captured as a [`FallbackTrigger`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminationReason {
    Success,
    AgentError,
    RateLimited,
    QuotaExhausted,
    TurnLimit,
    Timeout,
    ProcessCrash,
    CredentialError,
    Cancelled,
    InfrastructureError,
    ResourceExhausted,
    Unknown,
}

/// Lifecycle status of an individual agent execution within an attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentExecutionStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Interrupted,
}

/// Reason Orbit decided to trigger a fallback or continuation with another agent.
///
/// Distinct from [`TerminationReason`]: for example, an agent may terminate with
/// `TerminationReason::Success`, but external test validation fails, resulting in
/// `FallbackTrigger::ValidationFailed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackTrigger {
    AgentFailure,
    RateLimited,
    QuotaExhausted,
    TurnLimit,
    Timeout,
    ValidationFailed,
    InfrastructureFailure,
}

/// A single execution of an agent within an attempt's workspace.
///
/// An Attempt owns the workspace; an AgentExecution only uses it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentExecution {
    pub execution_id: String,
    pub sequence: u32,

    pub agent_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    pub started_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,

    pub status: AgentExecutionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub termination_reason: Option<TerminationReason>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,

    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: serde_json::Value,
}

impl AgentExecution {
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.execution_id.is_empty(), "execution_id required");
        ensure!(self.sequence > 0, "sequence must be >= 1");
        ensure!(!self.agent_type.is_empty(), "agent_type required");
        ensure!(self.started_at >= 0, "started_at must be non-negative");
        if let Some(finished) = self.finished_at {
            ensure!(
                finished >= self.started_at,
                "finished_at must be >= started_at"
            );
        }
        Ok(())
    }
}

/// Provider-neutral workspace snapshot capturing working tree state before a handoff.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSnapshot {
    pub baseline_revision: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub head_revision: String,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deleted_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub untracked_files: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_artifact_id: Option<String>,
}

impl WorkspaceSnapshot {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.baseline_revision.is_empty(),
            "baseline_revision required"
        );
        if let Some(sha) = &self.diff_sha256 {
            ensure!(crate::agent::valid_digest(sha), "invalid diff_sha256");
        }
        Ok(())
    }
}

/// Summary of previous execution context included in a handoff.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviousExecutionSummary {
    pub execution_id: String,
    pub agent_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub termination_reason: TerminationReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Summary reference to validation evidence for a handoff.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationSummary {
    pub command: String,
    pub exit_code: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_artifact_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_fingerprint: Option<FailureFingerprint>,
}

/// Versioned handoff structure ("handoff/v1") used to continue work with another agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffRecord {
    pub schema: String,
    pub task_id: String,
    pub attempt_id: String,
    pub from_execution_id: String,
    pub trigger: FallbackTrigger,

    pub workspace: WorkspaceSnapshot,
    pub previous_execution: PreviousExecutionSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation: Option<ValidationSummary>,

    pub created_at: i64,
}

pub const HANDOFF_SCHEMA_V1: &str = "handoff/v1";

impl HandoffRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        task_id: impl Into<String>,
        attempt_id: impl Into<String>,
        from_execution_id: impl Into<String>,
        trigger: FallbackTrigger,
        workspace: WorkspaceSnapshot,
        previous_execution: PreviousExecutionSummary,
        validation: Option<ValidationSummary>,
        created_at: i64,
    ) -> Result<Self> {
        let record = Self {
            schema: HANDOFF_SCHEMA_V1.into(),
            task_id: task_id.into(),
            attempt_id: attempt_id.into(),
            from_execution_id: from_execution_id.into(),
            trigger,
            workspace,
            previous_execution,
            validation,
            created_at,
        };
        record.validate()?;
        Ok(record)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == HANDOFF_SCHEMA_V1,
            "unsupported handoff schema"
        );
        ensure!(!self.task_id.is_empty(), "task_id required");
        ensure!(!self.attempt_id.is_empty(), "attempt_id required");
        ensure!(
            !self.from_execution_id.is_empty(),
            "from_execution_id required"
        );
        self.workspace.validate()?;
        ensure!(self.created_at >= 0, "created_at must be non-negative");
        Ok(())
    }
}

/// Normalized result of an agent execution produced by an adapter or runtime.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizedAgentResult {
    pub status: AgentExecutionStatus,
    pub termination_reason: TerminationReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: serde_json::Value,
}

impl NormalizedAgentResult {
    pub fn completed() -> Self {
        Self {
            status: AgentExecutionStatus::Completed,
            termination_reason: TerminationReason::Success,
            exit_code: Some(0),
            message: None,
            metadata: serde_json::Value::Null,
        }
    }

    pub fn turn_limit(message: impl Into<String>) -> Self {
        Self {
            status: AgentExecutionStatus::Interrupted,
            termination_reason: TerminationReason::TurnLimit,
            exit_code: None,
            message: Some(message.into()),
            metadata: serde_json::Value::Null,
        }
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self {
            status: AgentExecutionStatus::Interrupted,
            termination_reason: TerminationReason::Timeout,
            exit_code: None,
            message: Some(message.into()),
            metadata: serde_json::Value::Null,
        }
    }

    pub fn cancelled(message: impl Into<String>) -> Self {
        Self {
            status: AgentExecutionStatus::Interrupted,
            termination_reason: TerminationReason::Cancelled,
            exit_code: None,
            message: Some(message.into()),
            metadata: serde_json::Value::Null,
        }
    }

    pub fn rate_limited(message: impl Into<String>) -> Self {
        Self {
            status: AgentExecutionStatus::Interrupted,
            termination_reason: TerminationReason::RateLimited,
            exit_code: None,
            message: Some(message.into()),
            metadata: serde_json::Value::Null,
        }
    }

    pub fn quota_exhausted(message: impl Into<String>) -> Self {
        Self {
            status: AgentExecutionStatus::Interrupted,
            termination_reason: TerminationReason::QuotaExhausted,
            exit_code: None,
            message: Some(message.into()),
            metadata: serde_json::Value::Null,
        }
    }

    pub fn resource_exhausted(message: impl Into<String>) -> Self {
        Self {
            status: AgentExecutionStatus::Interrupted,
            termination_reason: TerminationReason::ResourceExhausted,
            exit_code: None,
            message: Some(message.into()),
            metadata: serde_json::Value::Null,
        }
    }

    pub fn process_crash(exit_code: Option<i32>, message: impl Into<String>) -> Self {
        Self {
            status: AgentExecutionStatus::Failed,
            termination_reason: TerminationReason::ProcessCrash,
            exit_code,
            message: Some(message.into()),
            metadata: serde_json::Value::Null,
        }
    }

    pub fn credential_error(message: impl Into<String>) -> Self {
        Self {
            status: AgentExecutionStatus::Failed,
            termination_reason: TerminationReason::CredentialError,
            exit_code: None,
            message: Some(message.into()),
            metadata: serde_json::Value::Null,
        }
    }

    pub fn infrastructure_error(message: impl Into<String>) -> Self {
        Self {
            status: AgentExecutionStatus::Failed,
            termination_reason: TerminationReason::InfrastructureError,
            exit_code: None,
            message: Some(message.into()),
            metadata: serde_json::Value::Null,
        }
    }

    pub fn agent_error(message: impl Into<String>) -> Self {
        Self {
            status: AgentExecutionStatus::Failed,
            termination_reason: TerminationReason::AgentError,
            exit_code: None,
            message: Some(message.into()),
            metadata: serde_json::Value::Null,
        }
    }

    pub fn unknown(message: impl Into<String>) -> Self {
        Self {
            status: AgentExecutionStatus::Failed,
            termination_reason: TerminationReason::Unknown,
            exit_code: None,
            message: Some(message.into()),
            metadata: serde_json::Value::Null,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn into_execution(
        self,
        execution_id: impl Into<String>,
        sequence: u32,
        agent_type: impl Into<String>,
        provider: Option<String>,
        model: Option<String>,
        started_at: i64,
        finished_at: Option<i64>,
    ) -> AgentExecution {
        AgentExecution {
            execution_id: execution_id.into(),
            sequence,
            agent_type: agent_type.into(),
            provider,
            model,
            started_at,
            finished_at,
            status: self.status,
            termination_reason: Some(self.termination_reason),
            exit_code: self.exit_code,
            message: self.message,
            metadata: self.metadata,
        }
    }
}

/// Classify HTTP 429 or resource exhaustion errors.
///
/// Priority:
/// 1. Known quota exhaustion -> QuotaExhausted
/// 2. Known temporary rate limit -> RateLimited
/// 3. Ambiguous resource exhaustion -> ResourceExhausted
/// 4. Otherwise -> AgentError / fallback
pub fn classify_http_429(error_text: &str, provider_code: Option<&str>) -> NormalizedAgentResult {
    let lower = error_text.to_ascii_lowercase();
    let code_lower = provider_code.unwrap_or("").to_ascii_lowercase();

    // Check provider structured code or unambiguous quota phrasing first
    if code_lower == "insufficient_quota"
        || code_lower == "quota_exceeded"
        || code_lower == "quota_exhausted"
        || lower.contains("insufficient_quota")
        || lower.contains("exceeded your current quota")
        || lower.contains("quota exceeded")
        || lower.contains("quota has been exhausted")
        || lower.contains("monthly limit")
        || lower.contains("usage limit")
    {
        return NormalizedAgentResult::quota_exhausted(error_text);
    }

    // Check temporary rate limit (tokens per minute, requests per minute)
    if code_lower == "rate_limit_exceeded"
        || code_lower == "requests_per_minute"
        || lower.contains("rate limit")
        || lower.contains("too many requests")
        || lower.contains("requests per minute")
        || lower.contains("tokens per minute")
        || lower.contains("try again in")
        || lower.contains("retry after")
    {
        return NormalizedAgentResult::rate_limited(error_text);
    }

    // Ambiguous resource exhaustion (e.g. gRPC RESOURCE_EXHAUSTED without subcode)
    if code_lower == "resource_exhausted" || lower.contains("resource_exhausted") {
        return NormalizedAgentResult::resource_exhausted(error_text);
    }

    NormalizedAgentResult::agent_error(error_text)
}

/// Normalizes Antigravity ACP adapter error conditions.
pub fn normalize_antigravity_error(error_text: &str) -> NormalizedAgentResult {
    let lower = error_text.to_ascii_lowercase();

    if lower.contains("turn timeout") || lower.contains("turn did not complete") {
        return NormalizedAgentResult::turn_limit(error_text);
    }
    if lower.contains("initialize timeout") || lower.contains("session creation timeout") {
        return NormalizedAgentResult::timeout(error_text);
    }
    if lower.contains("session/cancel") || lower.contains("cancelled") {
        return NormalizedAgentResult::cancelled(error_text);
    }
    if lower.contains("unauthenticated")
        || lower.contains("auth store")
        || lower.contains("quarantined")
        || lower.contains("token expired")
        || lower.contains("oauth")
    {
        return NormalizedAgentResult::credential_error(error_text);
    }
    if lower.contains("supervisor launch failed") || lower.contains("podman run") {
        return NormalizedAgentResult::infrastructure_error(error_text);
    }
    if lower.contains("429") || lower.contains("resource_exhausted") {
        return classify_http_429(error_text, None);
    }

    NormalizedAgentResult::agent_error(error_text)
}

/// Normalizes Codex ACP adapter error conditions.
pub fn normalize_codex_error(error_text: &str) -> NormalizedAgentResult {
    let lower = error_text.to_ascii_lowercase();

    if lower.contains("cancelled") {
        return NormalizedAgentResult::cancelled(error_text);
    }
    if lower.contains("codex setup failed") || lower.contains("codex turn failed") {
        // Inspect for quota or rate limit indicators
        if lower.contains("quota") || lower.contains("usage limit") {
            return NormalizedAgentResult::quota_exhausted(error_text);
        }
        if lower.contains("rate limit") || lower.contains("too many requests") {
            return NormalizedAgentResult::rate_limited(error_text);
        }
        return NormalizedAgentResult::agent_error(error_text);
    }
    if lower.contains("unauthorized")
        || lower.contains("invalid_api_key")
        || lower.contains("auth.json")
    {
        return NormalizedAgentResult::credential_error(error_text);
    }
    if lower.contains("turn timeout") {
        return NormalizedAgentResult::turn_limit(error_text);
    }
    if lower.contains("supervisor launch failed") || lower.contains("podman run") {
        return NormalizedAgentResult::infrastructure_error(error_text);
    }
    if lower.contains("429") {
        return classify_http_429(error_text, None);
    }

    NormalizedAgentResult::agent_error(error_text)
}

/// Normalizes ACP process exit status.
pub fn normalize_acp_process_exit(
    exit_code: Option<i32>,
    expected_clean: bool,
    error_detail: Option<&str>,
) -> NormalizedAgentResult {
    match exit_code {
        Some(0) if expected_clean => NormalizedAgentResult::completed(),
        Some(code) => {
            let msg = error_detail
                .unwrap_or("ACP container or supervisor process exited with non-zero code");
            NormalizedAgentResult::process_crash(Some(code), format!("{msg} (exit code {code})"))
        }
        None => {
            let msg =
                error_detail.unwrap_or("ACP container or supervisor process terminated by signal");
            NormalizedAgentResult::process_crash(None, msg)
        }
    }
}

/// Builds a provider-neutral continuation prompt for the next agent.
///
/// Instructs the receiving agent on:
/// 1. The original task requirement
/// 2. The previous agent's execution outcome and reason for continuation
/// 3. The current repository state (changed, added, deleted, untracked files)
/// 4. Recent validation results and diagnostics, if any
/// 5. Standard continuation directives: inspect diff, keep valid work, fix defects, complete task
///
/// Note: Complete git diffs and raw logs are deliberately omitted from the prompt.
/// The agent has direct access to the live workspace repository.
pub fn build_handoff_prompt(
    original_task: &str,
    handoff_record: &HandoffRecord,
    validation_summary: Option<&ValidationSummary>,
) -> String {
    use std::fmt::Write;
    let mut prompt = String::new();

    writeln!(
        prompt,
        "You are continuing an existing implementation attempt."
    )
    .unwrap();
    writeln!(
        prompt,
        "
Original task:
{}",
        original_task.trim()
    )
    .unwrap();
    writeln!(
        prompt,
        "
A previous coding agent worked on this repository."
    )
    .unwrap();
    writeln!(prompt, "The current workspace contains that agent's changes. Do not discard those changes automatically.").unwrap();

    let prev = &handoff_record.previous_execution;
    writeln!(
        prompt,
        "
Previous execution:"
    )
    .unwrap();
    writeln!(prompt, "Agent: {}", prev.agent_type).unwrap();
    if let Some(model) = &prev.model {
        writeln!(prompt, "Model: {}", model).unwrap();
    }
    writeln!(prompt, "Termination: {:?}", prev.termination_reason).unwrap();
    if let Some(msg) = &prev.message {
        let trimmed_msg = if msg.len() > 500 {
            &msg[..500]
        } else {
            msg.as_str()
        };
        writeln!(prompt, "Diagnostic: {}", trimmed_msg).unwrap();
    }

    let ws = &handoff_record.workspace;
    writeln!(
        prompt,
        "
Repository state:"
    )
    .unwrap();
    writeln!(prompt, "Baseline: {}", ws.baseline_revision).unwrap();
    if !ws.head_revision.is_empty() {
        writeln!(prompt, "HEAD: {}", ws.head_revision).unwrap();
    }

    if !ws.changed_files.is_empty() {
        writeln!(
            prompt,
            "
Changed files:"
        )
        .unwrap();
        for f in &ws.changed_files {
            writeln!(prompt, "- {}", f).unwrap();
        }
    }
    if !ws.added_files.is_empty() {
        writeln!(
            prompt,
            "
Added files:"
        )
        .unwrap();
        for f in &ws.added_files {
            writeln!(prompt, "- {}", f).unwrap();
        }
    }
    if !ws.deleted_files.is_empty() {
        writeln!(
            prompt,
            "
Deleted files:"
        )
        .unwrap();
        for f in &ws.deleted_files {
            writeln!(prompt, "- {}", f).unwrap();
        }
    }
    if !ws.untracked_files.is_empty() {
        writeln!(
            prompt,
            "
Untracked files:"
        )
        .unwrap();
        for f in &ws.untracked_files {
            writeln!(prompt, "- {}", f).unwrap();
        }
    }

    let val = validation_summary.or(handoff_record.validation.as_ref());
    if let Some(v) = val {
        writeln!(
            prompt,
            "
Latest validation:"
        )
        .unwrap();
        writeln!(prompt, "Command: {}", v.command).unwrap();
        writeln!(prompt, "Exit code: {}", v.exit_code).unwrap();
        if let Some(summary) = &v.summary {
            let bounded_summary = if summary.len() > 1000 {
                &summary[..1000]
            } else {
                summary.as_str()
            };
            writeln!(
                prompt,
                "
Failure summary:
{}",
                bounded_summary.trim()
            )
            .unwrap();
        }
    }

    writeln!(
        prompt,
        "
Instructions:"
    )
    .unwrap();
    writeln!(prompt, "1. Inspect the existing repository and git diff.").unwrap();
    writeln!(prompt, "2. Understand the previous changes before editing.").unwrap();
    writeln!(prompt, "3. Keep correct existing work.").unwrap();
    writeln!(prompt, "4. Correct incomplete or incorrect changes.").unwrap();
    writeln!(prompt, "5. Complete the original task.").unwrap();
    writeln!(prompt, "6. Run the required validation when possible.").unwrap();
    writeln!(
        prompt,
        "
The repository and Orbit validation evidence are authoritative."
    )
    .unwrap();
    writeln!(
        prompt,
        "Do not assume the previous agent's implementation is correct."
    )
    .unwrap();

    prompt
}

/// Policy configuring automatic cross-agent continuation and fallback.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FallbackPolicy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_agent: Option<String>,
    #[serde(default = "default_fallback_triggers")]
    pub on_triggers: Vec<FallbackTrigger>,
    #[serde(default = "default_max_executions")]
    pub max_executions: u32,
}

fn default_fallback_triggers() -> Vec<FallbackTrigger> {
    vec![
        FallbackTrigger::TurnLimit,
        FallbackTrigger::RateLimited,
        FallbackTrigger::QuotaExhausted,
        FallbackTrigger::Timeout,
        FallbackTrigger::ValidationFailed,
    ]
}

fn default_max_executions() -> u32 {
    2
}

impl Default for FallbackPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            fallback_agent: None,
            on_triggers: default_fallback_triggers(),
            max_executions: 2,
        }
    }
}

/// Evaluates whether an agent execution and its subsequent external validation trigger a fallback.
///
/// Cancellation, CredentialError, InfrastructureError, and ProcessCrash do NOT trigger fallback
/// in this phase to prevent uncontrolled retries of fundamental environment failures.
pub fn fallback_trigger(
    execution: &AgentExecution,
    validation: Option<&ValidationSummary>,
    policy: &FallbackPolicy,
) -> Option<FallbackTrigger> {
    if !policy.enabled {
        return None;
    }

    // Cancellation has absolute priority: no fallback
    if execution.status == AgentExecutionStatus::Interrupted
        && execution.termination_reason == Some(TerminationReason::Cancelled)
    {
        return None;
    }

    // Explicit exclusions for safety:
    if let Some(reason) = execution.termination_reason
        && matches!(
            reason,
            TerminationReason::Cancelled
                | TerminationReason::CredentialError
                | TerminationReason::InfrastructureError
                | TerminationReason::ProcessCrash
        )
    {
        return None;
    }

    // Check agent termination reasons
    let candidate = match execution.termination_reason {
        Some(TerminationReason::TurnLimit) => Some(FallbackTrigger::TurnLimit),
        Some(TerminationReason::RateLimited) => Some(FallbackTrigger::RateLimited),
        Some(TerminationReason::QuotaExhausted) => Some(FallbackTrigger::QuotaExhausted),
        Some(TerminationReason::ResourceExhausted) => Some(FallbackTrigger::RateLimited),
        Some(TerminationReason::Timeout) => Some(FallbackTrigger::Timeout),
        Some(TerminationReason::AgentError) => Some(FallbackTrigger::AgentFailure),
        _ => None,
    };

    if let Some(trigger) = candidate
        && policy.on_triggers.contains(&trigger)
    {
        return Some(trigger);
    }

    // External validation failure check (e.g. agent completed but tests failed)
    if let Some(val) = validation
        && val.exit_code != 0
        && policy
            .on_triggers
            .contains(&FallbackTrigger::ValidationFailed)
    {
        return Some(FallbackTrigger::ValidationFailed);
    }

    None
}

/// Explicit action derived during crash recovery or reconciler passes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ContinuationRecoveryAction {
    /// No continuation action needed (e.g. terminal attempt, disabled policy, or attempt completed).
    None,

    /// Agent execution terminated, but external validation evidence is missing.
    ResumeValidation { execution_id: String },

    /// Validation failed or agent terminated with a fallback trigger, but handoff record is missing.
    PrepareHandoff {
        execution_id: String,
        trigger: FallbackTrigger,
    },

    /// Handoff record exists and fallback agent needs to be launched.
    StartFallback {
        handoff_id: String,
        sequence: u32,
        agent: String,
    },

    /// A fallback execution is already pending and needs to be claimed/launched.
    ClaimPendingExecution { execution_id: String, sequence: u32 },

    /// An execution is currently marked running; verify worker/lease liveness.
    ReconcileRunningExecution { execution_id: String },

    /// Fallback budget exhausted, safety exclusion met, or workspace unrecoverable.
    FinalizeFailure { reason: String },
}

/// Pure deterministic decision function for continuation crash recovery.
///
/// Derives the next continuation action strictly from durable state:
/// - Attempt status and cancellation
/// - Sequence of persisted AgentExecutions
/// - Persisted ValidationSummary records
/// - Persisted HandoffRecords
/// - FallbackPolicy
///
/// Ensures idempotency, bounded executions (max = 2), and fail-closed safety.
pub fn continuation_recovery_action(
    attempt_state: &crate::model::State,
    executions: &[AgentExecution],
    validations: &[ValidationSummary],
    handoffs: &[HandoffRecord],
    policy: &FallbackPolicy,
    current_workspace_diff_sha256: Option<&str>,
) -> ContinuationRecoveryAction {
    // 1. Terminal Attempt Protection: If Attempt is terminal, recovery is a no-op.
    if attempt_state.terminal() {
        return ContinuationRecoveryAction::None;
    }

    // 2. Cancellation has absolute priority.
    if *attempt_state == crate::model::State::CancelRequested
        || *attempt_state == crate::model::State::Cancelled
    {
        return ContinuationRecoveryAction::None;
    }

    // 3. If continuation is disabled, do not perform any continuation recovery.
    if !policy.enabled {
        return ContinuationRecoveryAction::None;
    }

    // 4. If no executions exist yet, initial primary agent has not started.
    let last_exec = match executions.last() {
        Some(e) => e,
        None => return ContinuationRecoveryAction::None,
    };

    // If any execution was cancelled, cancel continuation immediately.
    if last_exec.status == AgentExecutionStatus::Interrupted
        && last_exec.termination_reason == Some(TerminationReason::Cancelled)
    {
        return ContinuationRecoveryAction::None;
    }

    // 5. Check if the latest execution is still Running or Pending.
    if last_exec.status == AgentExecutionStatus::Running {
        return ContinuationRecoveryAction::ReconcileRunningExecution {
            execution_id: last_exec.execution_id.clone(),
        };
    }

    if last_exec.status == AgentExecutionStatus::Pending {
        return ContinuationRecoveryAction::ClaimPendingExecution {
            execution_id: last_exec.execution_id.clone(),
            sequence: last_exec.sequence,
        };
    }

    // 6. Latest execution is terminal.
    // If the latest execution succeeded and passed validation, attempt is complete!
    let matching_val = validations
        .iter()
        .find(|v| v.summary.as_deref().is_some() || v.exit_code == 0 || v.exit_code != 0);

    // If sequence == 2 (fallback already ran) or executions >= max_executions
    if last_exec.sequence >= policy.max_executions
        || executions.len() as u32 >= policy.max_executions
    {
        if last_exec.status == AgentExecutionStatus::Completed
            && matching_val.is_some_and(|v| v.exit_code == 0)
        {
            return ContinuationRecoveryAction::None;
        }
        return ContinuationRecoveryAction::FinalizeFailure {
            reason: format!(
                "Max executions reached ({}/{}); fallback budget exhausted",
                executions.len(),
                policy.max_executions
            ),
        };
    }

    // 7. Sequence == 1: Evaluate fallback trigger.
    let trigger = fallback_trigger(last_exec, matching_val, policy);

    let Some(trigger) = trigger else {
        // If primary succeeded and validation passed, no action needed (will finalize success)
        if last_exec.status == AgentExecutionStatus::Completed
            && matching_val.is_some_and(|v| v.exit_code == 0)
        {
            return ContinuationRecoveryAction::None;
        }

        // If primary finished but validation has not run yet:
        if matching_val.is_none() && last_exec.status == AgentExecutionStatus::Completed {
            return ContinuationRecoveryAction::ResumeValidation {
                execution_id: last_exec.execution_id.clone(),
            };
        }

        // Otherwise, terminated with a reason that is excluded from fallback (e.g. CredentialError, Cancelled)
        return ContinuationRecoveryAction::FinalizeFailure {
            reason: format!(
                "Agent execution terminated with {:?}; not eligible for fallback",
                last_exec.termination_reason
            ),
        };
    };

    // 8. Fallback triggered: Check if HandoffRecord exists.
    let existing_handoff = handoffs
        .iter()
        .find(|h| h.from_execution_id == last_exec.execution_id);

    let Some(handoff) = existing_handoff else {
        return ContinuationRecoveryAction::PrepareHandoff {
            execution_id: last_exec.execution_id.clone(),
            trigger,
        };
    };

    // 9. Workspace Staleness Check:
    // If a current workspace diff SHA is provided, verify it matches the persisted handoff.
    if let (Some(current_sha), Some(expected_sha)) = (
        current_workspace_diff_sha256,
        handoff.workspace.diff_sha256.as_deref(),
    ) && current_sha != expected_sha
    {
        return ContinuationRecoveryAction::FinalizeFailure {
            reason: format!(
                "Workspace drift detected: expected diff digest {expected_sha}, found {current_sha}"
            ),
        };
    }

    // 10. Handoff is ready and valid: Start fallback agent with sequence = 2
    let fallback_agent = policy
        .fallback_agent
        .clone()
        .unwrap_or_else(|| "codex".into());

    ContinuationRecoveryAction::StartFallback {
        handoff_id: handoff.from_execution_id.clone(),
        sequence: last_exec.sequence + 1,
        agent: fallback_agent,
    }
}

/// Algorithm version for validation failure fingerprints.
pub const FINGERPRINT_VERSION_VALIDATION_V1: &str = "validation/v1";

/// Kind of failure fingerprint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureFingerprintKind {
    Validation,
    AgentTermination,
}

/// Deterministic, normalized fingerprint of an implementation failure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailureFingerprint {
    pub version: String,
    pub kind: FailureFingerprintKind,
    pub digest: String,
    pub summary: String,
}

impl FailureFingerprint {
    pub fn new(
        version: impl Into<String>,
        kind: FailureFingerprintKind,
        digest: impl Into<String>,
        summary: impl Into<String>,
    ) -> Result<Self> {
        let digest_str = digest.into();
        ensure!(digest_str.len() == 64, "invalid fingerprint digest length");
        Ok(Self {
            version: version.into(),
            kind,
            digest: digest_str,
            summary: summary.into(),
        })
    }
}

/// Normalizes raw validation diagnostic text removing volatile values:
/// - Timestamps (e.g. `2026-09-20T...`, `HH:MM:SS`)
/// - Absolute workspace paths (e.g. `/tmp/.../src/` -> `src/`)
/// - Line and column numbers (e.g. `:42:15` -> ``)
/// - ANSI escape codes
/// - Attempt and execution IDs
pub fn normalize_diagnostic_text(raw: &str) -> String {
    // 1. Strip ANSI escape codes
    let mut cleaned = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b'
            && let Some(&'[') = chars.peek()
        {
            chars.next();
            for next_c in chars.by_ref() {
                if next_c.is_ascii_alphabetic() || next_c == 'm' {
                    break;
                }
            }
            continue;
        }
        cleaned.push(c);
    }

    // 2. Canonicalize line by line
    let mut canonical_lines = Vec::new();
    for line in cleaned.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Ignore compiler status / logging lines
        if trimmed.contains("[INFO]")
            || trimmed.contains("[DEBUG]")
            || trimmed.contains("[WARN]")
            || trimmed.starts_with("Compiling")
            || trimmed.starts_with("Finished")
            || trimmed.starts_with("Running")
        {
            continue;
        }

        // Strip ISO8601 timestamps like 2026-09-20T...
        let mut filtered = trimmed.to_string();
        if let Some(pos) = filtered.find("202")
            && filtered.len() >= pos + 20
            && filtered.as_bytes()[pos + 4] == b'-'
        {
            filtered.replace_range(pos..pos + 20, "");
        }

        // Normalize absolute paths: keep path starting from known components like "src/", "tests/", or basename
        if let Some(idx) = filtered.find("/src/") {
            filtered = filtered[idx + 1..].to_string();
        } else if let Some(idx) = filtered.find("/tests/") {
            filtered = filtered[idx + 1..].to_string();
        } else if let Some(idx) = filtered.find("/tmp/")
            && let Some(after) = filtered[idx..].find("src/")
        {
            filtered = filtered[idx + after..].to_string();
        }

        // Strip line numbers like :42:15 or :42
        // Strip leading line numbers like "42 |" -> "|"
        if let Some(pipe_pos) = filtered.find(" |") {
            let prefix = filtered[..pipe_pos].trim();
            if prefix.chars().all(|c| c.is_ascii_digit()) {
                filtered = filtered[pipe_pos + 1..].to_string();
            }
        }

        let parts: Vec<&str> = filtered.split_whitespace().collect();
        let mut scrubbed_parts = Vec::new();
        for p in parts {
            if let Some(colon_pos) = p.find(':') {
                let prefix = &p[..colon_pos];
                if prefix.ends_with(".rs") || prefix.ends_with(".toml") {
                    scrubbed_parts.push(prefix.to_string());
                    continue;
                }
            }
            scrubbed_parts.push(p.to_string());
        }

        let canonical_line = scrubbed_parts.join(" ");
        if !canonical_line.is_empty() {
            canonical_lines.push(canonical_line);
        }
    }

    canonical_lines.join("\n")
}

/// Generates a deterministic validation FailureFingerprint from command, exit code, and diagnostic output.
pub fn generate_validation_fingerprint(
    command: &str,
    exit_code: i32,
    diagnostic_output: &str,
) -> FailureFingerprint {
    let normalized = normalize_diagnostic_text(diagnostic_output);
    let canonical = format!(
        "command={}\nexit_code={}\nnormalized_diagnostic={}",
        command.trim(),
        exit_code,
        normalized
    );

    let digest = crate::model::digest(canonical.as_bytes());
    let summary = normalized
        .lines()
        .next()
        .unwrap_or("validation failure")
        .chars()
        .take(80)
        .collect();

    FailureFingerprint {
        version: FINGERPRINT_VERSION_VALIDATION_V1.into(),
        kind: FailureFingerprintKind::Validation,
        digest,
        summary,
    }
}

/// Counts how many times the given fingerprint has been observed in previous validations.
pub fn repeated_failure_count(
    fingerprint: &FailureFingerprint,
    previous_validations: &[ValidationSummary],
) -> usize {
    let mut count = 0;
    for val in previous_validations {
        if let Some(ref fp) = val.failure_fingerprint
            && fp.version == fingerprint.version
            && fp.digest == fingerprint.digest
        {
            count += 1;
        }
    }
    count
}

/// Candidate agent in an ordered execution chain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentCandidate {
    pub id: String,
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl AgentCandidate {
    pub fn new(id: impl Into<String>, agent: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            agent: agent.into(),
            provider: None,
            model: None,
        }
    }
}

/// Generalized policy for cross-agent continuation chains.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContinuationPolicy {
    #[serde(default)]
    pub enabled: bool,

    #[serde(default)]
    pub agents: Vec<AgentCandidate>,

    #[serde(default = "default_max_executions")]
    pub max_executions: u32,

    #[serde(default = "default_fallback_triggers")]
    pub triggers: Vec<FallbackTrigger>,

    #[serde(default = "default_max_repetitions")]
    pub max_same_failure_repetitions: u32,
}

fn default_max_repetitions() -> u32 {
    2
}

impl Default for ContinuationPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            agents: Vec::new(),
            max_executions: 3,
            triggers: default_fallback_triggers(),
            max_same_failure_repetitions: 2,
        }
    }
}

impl From<FallbackPolicy> for ContinuationPolicy {
    fn from(fb: FallbackPolicy) -> Self {
        let mut agents = Vec::new();
        if let Some(fallback) = fb.fallback_agent {
            agents.push(AgentCandidate::new("primary", "antigravity"));
            agents.push(AgentCandidate::new("fallback", fallback));
        }
        Self {
            enabled: fb.enabled,
            agents,
            max_executions: fb.max_executions,
            triggers: fb.on_triggers,
            max_same_failure_repetitions: 2,
        }
    }
}

/// Next agent routing decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum NextAgentDecision {
    StopSuccess,
    StopFailure {
        reason: String,
    },
    Continue {
        candidate_index: usize,
        candidate: AgentCandidate,
        sequence: u32,
        trigger: FallbackTrigger,
        reason: String,
    },
}

/// Pure deterministic decision function for selecting the next agent in the continuation chain.
pub fn next_agent(
    attempt_state: &crate::model::State,
    executions: &[AgentExecution],
    validations: &[ValidationSummary],
    policy: &ContinuationPolicy,
) -> NextAgentDecision {
    if attempt_state.terminal() {
        return NextAgentDecision::StopSuccess;
    }
    if *attempt_state == crate::model::State::CancelRequested
        || *attempt_state == crate::model::State::Cancelled
    {
        return NextAgentDecision::StopFailure {
            reason: "Attempt cancelled".into(),
        };
    }
    if !policy.enabled || policy.agents.is_empty() {
        return NextAgentDecision::StopFailure {
            reason: "Continuation policy disabled or no agents configured".into(),
        };
    }

    let Some(last_exec) = executions.last() else {
        // Initial execution
        return NextAgentDecision::Continue {
            candidate_index: 0,
            candidate: policy.agents[0].clone(),
            sequence: 1,
            trigger: FallbackTrigger::TurnLimit,
            reason: "Initial agent execution".into(),
        };
    };

    // Cancellation priority
    if last_exec.status == AgentExecutionStatus::Interrupted
        && last_exec.termination_reason == Some(TerminationReason::Cancelled)
    {
        return NextAgentDecision::StopFailure {
            reason: "Agent execution was cancelled".into(),
        };
    }

    let matching_val = validations.last();

    // Success check
    if last_exec.status == AgentExecutionStatus::Completed
        && matching_val.is_some_and(|v| v.exit_code == 0)
    {
        return NextAgentDecision::StopSuccess;
    }

    // Check hard max execution limit
    if (executions.len() as u32) >= policy.max_executions {
        return NextAgentDecision::StopFailure {
            reason: format!(
                "Max executions reached ({}/{}); chain exhausted",
                executions.len(),
                policy.max_executions
            ),
        };
    }

    // Determine fallback trigger
    let fb_policy = FallbackPolicy {
        enabled: policy.enabled,
        fallback_agent: None,
        on_triggers: policy.triggers.clone(),
        max_executions: policy.max_executions,
    };
    let Some(trigger) = fallback_trigger(last_exec, matching_val, &fb_policy) else {
        return NextAgentDecision::StopFailure {
            reason: format!(
                "Agent terminated with {:?}; not eligible for continuation",
                last_exec.termination_reason
            ),
        };
    };

    // Calculate next candidate index
    let current_index = (last_exec.sequence as usize).saturating_sub(1);
    let next_index = current_index + 1;

    // Check failure repetition if validation failed
    let mut routing_reason = format!("Trigger {:?} occurred", trigger);
    if let Some(val) = matching_val
        && let Some(ref fp) = val.failure_fingerprint
    {
        let reps = repeated_failure_count(fp, validations);
        if reps >= policy.max_same_failure_repetitions as usize {
            routing_reason = format!(
                "Failure fingerprint {} repeated {} times (threshold {}); advancing candidate",
                fp.digest.chars().take(8).collect::<String>(),
                reps,
                policy.max_same_failure_repetitions
            );
        }
    }

    if next_index >= policy.agents.len() {
        return NextAgentDecision::StopFailure {
            reason: "candidate_chain_exhausted".into(),
        };
    }

    NextAgentDecision::Continue {
        candidate_index: next_index,
        candidate: policy.agents[next_index].clone(),
        sequence: last_exec.sequence + 1,
        trigger,
        reason: routing_reason,
    }
}
