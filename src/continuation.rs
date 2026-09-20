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
