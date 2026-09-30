//! Live ACP role execution, lifecycle persistence, and supervisor cleanup.

use super::*;

/// Outcome of a role agent execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleExecutionOutcome {
    pub raw_output: String,
    pub agent_execution_ids: Vec<String>,
    pub termination_reason: Option<String>,
}

/// Abstract executor for running role agents.
#[async_trait::async_trait]
pub trait RoleAgentExecutor: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    async fn execute_role(
        &self,
        pool: &PgPool,
        wf_run: &WorkflowRun,
        role_exec: &RoleExecution,
        role: &RoleDefinition,
        target: &ResolvedExecutionTarget,
        task_text: &str,
        repo_path: &Path,
        input_handoff: Option<&HandoffArtifact>,
        cancellation: tokio::sync::watch::Receiver<bool>,
    ) -> Result<RoleExecutionOutcome>;
}

async fn acp_call(
    wire: &mut Wire,
    state: &mut AcpTurnState<'_>,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    let id = wire.request(method, params).await?;
    loop {
        let value = wire.read().await?;
        if value.get("method").is_some() {
            handle_acp_message(wire, state, value).await?;
        } else {
            return Wire::result(value, &id);
        }
    }
}

fn validate_role_credential_target(
    target: &ResolvedExecutionTarget,
    credential: &crate::credential_registry::Credential,
) -> Result<()> {
    let generation = target
        .credential_generation
        .context("CREDENTIAL_PIN_REQUIRED")?;
    ensure!(
        target.credential_id.as_deref() == Some(credential.reference.as_str())
            && credential.provider == target.provider
            && credential.generation == u64::from(generation)
            && credential.status == crate::credential_registry::CredentialStatus::Enrolled,
        "CREDENTIAL_GENERATION_CHANGED: selected credential reference, provider, status, or generation changed before execution"
    );
    Ok(())
}

struct RoleModelEvidence<'a> {
    requested: Option<&'a str>,
    configured: Option<&'a str>,
    observed: Option<&'a str>,
}

fn role_model_evidence<'a>(
    target: &'a ResolvedExecutionTarget,
    observed: Option<&'a str>,
) -> RoleModelEvidence<'a> {
    RoleModelEvidence {
        requested: target.requested_model.as_deref(),
        configured: target.resolved_model.as_deref(),
        observed,
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub(super) struct AcpRoleLifecycle {
    pub(super) schema_version: u8,
    pub(super) phase: &'static str,
    pub(super) attempted_phase: &'static str,
    pub(super) failed_phase: Option<&'static str>,
    pub(super) last_confirmed_phase: &'static str,
    pub(super) milestones: Vec<&'static str>,
    pub(super) outcome: &'static str,
    pub(super) normalized_reason: Option<&'static str>,
    pub(super) prompt_uncertainty: &'static str,
    pub(super) cleanup_state: &'static str,
    pub(super) persistence_state: &'static str,
    pub(super) process_exit_code: Option<i32>,
    pub(super) process_signal: Option<i32>,
    pub(super) supervisor_outcome: &'static str,
    pub(super) supervisor_failure: Option<&'static str>,
    pub(super) supervisor_receipt: Option<crate::acp_process::CleanupReceiptEvidence>,
    pub(super) tool_audit_applicability: &'static str,
    #[serde(skip)]
    pub(super) finalization_attempted: bool,
}

impl AcpRoleLifecycle {
    pub(super) fn new() -> Self {
        Self {
            schema_version: 1,
            phase: "AGENT_EXECUTION_CREATED",
            attempted_phase: "AGENT_EXECUTION_CREATED",
            failed_phase: None,
            last_confirmed_phase: "AGENT_EXECUTION_CREATED",
            milestones: vec!["TARGET_COMMITTED", "AGENT_EXECUTION_CREATED"],
            outcome: "IN_PROGRESS",
            normalized_reason: None,
            prompt_uncertainty: "NOT_DISPATCHED",
            cleanup_state: "NO_RUNTIME_RESOURCE_CREATED",
            persistence_state: "CONFIRMED",
            process_exit_code: None,
            process_signal: None,
            supervisor_outcome: "NOT_OBSERVED",
            supervisor_failure: None,
            supervisor_receipt: None,
            tool_audit_applicability: "NOT_APPLICABLE_BEFORE_TOOL_PHASE",
            finalization_attempted: false,
        }
    }

    pub(super) fn enter(&mut self, phase: &'static str) {
        self.phase = phase;
        self.attempted_phase = phase;
    }

    pub(super) fn confirm(&mut self, phase: &'static str) {
        self.phase = phase;
        self.attempted_phase = phase;
        self.last_confirmed_phase = phase;
        if self.milestones.last().copied() != Some(phase) && self.milestones.len() < 16 {
            self.milestones.push(phase);
        }
    }

    pub(super) fn note_tool_activity(&mut self) {
        self.tool_audit_applicability = "APPLICABLE";
        const OBSERVED: &str = "TOOL_ACTIVITY_OBSERVED";
        if !self.milestones.contains(&OBSERVED) && self.milestones.len() < 16 {
            self.milestones.push(OBSERVED);
        }
    }

    pub(super) fn normalized_failure(&self, error: &anyhow::Error) -> &'static str {
        if error.is::<RoleLifecyclePersistenceFailed>() {
            return "AGENT_EXECUTION_LIFECYCLE_PERSISTENCE_FAILED";
        }
        if error.is::<RoleTerminalPersistenceUnconfirmed>() {
            return "AGENT_EXECUTION_TERMINAL_PERSISTENCE_UNCONFIRMED";
        }
        if let Some(error) = error.downcast_ref::<RoleSupervisorOutcomeFailure>() {
            return error.0;
        }
        if error.is::<RoleHandoffResponseMissing>() {
            return "HANDOFF_RESPONSE_MISSING";
        }
        if error.is::<crate::tools::budget::ToolBudgetExhausted>() {
            return "TOOL_BUDGET_EXHAUSTED";
        }
        if error.is::<RoleTerminalCleanupFailed>() {
            return "TERMINAL_CLEANUP_FAILED";
        }
        if error.is::<crate::acp_runtime::TurnTimeout>() {
            return "ACP_PROMPT_TIMEOUT";
        }
        if error.is::<RoleExecutionCancelled>() {
            return "ROLE_EXECUTION_CANCELLED";
        }
        if error.is::<RoleSupervisorTimeout>() {
            return "SUPERVISOR_TIMEOUT";
        }
        match self.phase {
            "CREDENTIAL_RESOLUTION" => "CREDENTIAL_RESOLUTION_FAILED",
            "CREDENTIAL_STAGING" => "CREDENTIAL_STAGING_FAILED",
            "RUNTIME_PREPARATION" => "RUNTIME_PREPARATION_FAILED",
            "SUPERVISOR_START" => "SUPERVISOR_START_FAILED",
            "ACP_INITIALIZE" => "ACP_INITIALIZE_FAILED",
            "SESSION_CREATION" => "ACP_SESSION_CREATION_FAILED",
            "PROMPT_DISPATCH" | "PROMPT_IN_FLIGHT" => "ACP_PROMPT_FAILED",
            "CLEANUP" => "ACP_CLEANUP_FAILED",
            "HANDOFF" => "HANDOFF_PARSE_FAILED",
            _ => "ACP_EXECUTION_FAILED",
        }
    }

    pub(super) fn record_error(&mut self, error: &anyhow::Error) {
        let reason = self.normalized_failure(error);
        self.failed_phase = Some(self.phase);
        if reason == "ACP_PROMPT_TIMEOUT" {
            let pending = error
                .downcast_ref::<crate::acp_runtime::TurnTimeout>()
                .and_then(|timeout| timeout.pending_model_call);
            self.prompt_uncertainty = match pending {
                Some(true) => "UNRESOLVED_PENDING_MODEL_CALL",
                Some(false) => "UNRESOLVED_NO_PENDING_MODEL_CALL",
                None => "UNRESOLVED_UNKNOWN",
            };
        } else if self.prompt_uncertainty == "IN_FLIGHT" {
            self.prompt_uncertainty = "UNRESOLVED_UNKNOWN";
        }
        self.outcome = "FAILED";
        self.normalized_reason = Some(reason);
        self.phase = "TERMINAL";
    }

    pub(super) fn finish_success(&mut self) {
        if self.prompt_uncertainty == "IN_FLIGHT" {
            self.prompt_uncertainty = "RESOLVED";
        }
        self.outcome = "SUCCEEDED";
        self.normalized_reason = Some("COMPLETED");
        self.phase = "TERMINAL";
        self.attempted_phase = "TERMINAL";
    }

    pub(super) fn restore_after_persistence_failure(
        &mut self,
        previous: &Self,
        attempted_phase: &'static str,
    ) {
        *self = previous.clone();
        self.persistence_state = "UNCONFIRMED";
        self.enter(attempted_phase);
    }

    pub(super) fn preserve_cleanup_state_for_failure_finalization(&mut self) {
        if !matches!(
            self.cleanup_state,
            "NO_RUNTIME_RESOURCE_CREATED" | "CONFIRMED"
        ) {
            self.cleanup_state = "UNCONFIRMED";
        }
    }

    pub(super) fn needs_failure_finalization(&self) -> bool {
        !self.finalization_attempted
    }

    pub(super) fn note_terminal_persistence_attempt(&mut self) {
        self.finalization_attempted = true;
    }

    pub(super) fn note_terminal_persistence_failure(&mut self) {
        self.finalization_attempted = true;
        self.persistence_state = "UNCONFIRMED";
    }

    pub(super) fn value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("bounded ACP lifecycle serializes")
    }
}

#[derive(Debug)]
pub(super) struct SupervisorEvidence {
    pub(super) exit_code: Option<i32>,
    pub(super) signal: Option<i32>,
    pub(super) cleanup_confirmed: bool,
    pub(super) failure: Option<&'static str>,
    pub(super) receipt: Option<crate::acp_process::CleanupReceiptEvidence>,
}

#[derive(Debug)]
pub(super) struct RoleExecutionCancelled;

impl std::fmt::Display for RoleExecutionCancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("role execution cancelled")
    }
}

impl std::error::Error for RoleExecutionCancelled {}

#[derive(Debug)]
pub(super) struct RoleSupervisorTimeout;

impl std::fmt::Display for RoleSupervisorTimeout {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("role supervisor deadline elapsed")
    }
}

impl std::error::Error for RoleSupervisorTimeout {}

#[derive(Debug)]
pub(super) struct RoleLifecyclePersistenceFailed;

impl std::fmt::Display for RoleLifecyclePersistenceFailed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("agent execution lifecycle evidence persistence failed")
    }
}

impl std::error::Error for RoleLifecyclePersistenceFailed {}

#[derive(Debug)]
pub(super) struct RoleTerminalPersistenceUnconfirmed;

impl std::fmt::Display for RoleTerminalPersistenceUnconfirmed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("terminal AgentExecution persistence is unconfirmed")
    }
}

impl std::error::Error for RoleTerminalPersistenceUnconfirmed {}

#[derive(Debug)]
pub(super) struct RoleSupervisorOutcomeFailure(pub(super) &'static str);

impl std::fmt::Display for RoleSupervisorOutcomeFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for RoleSupervisorOutcomeFailure {}

#[derive(Debug)]
pub(super) struct RoleHandoffResponseMissing;

impl std::fmt::Display for RoleHandoffResponseMissing {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("handoff response envelope missing")
    }
}

impl std::error::Error for RoleHandoffResponseMissing {}

#[derive(Debug)]
pub(super) struct RoleTerminalCleanupFailed;

impl std::fmt::Display for RoleTerminalCleanupFailed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("terminal cleanup failed")
    }
}

impl std::error::Error for RoleTerminalCleanupFailed {}

pub(super) async fn persist_agent_lifecycle_phase(
    store: &WorkflowStore,
    lifecycle: &mut AcpRoleLifecycle,
    agent_execution_id: &str,
    role_execution_id: &str,
    phase: &'static str,
) -> Result<()> {
    let previous = lifecycle.clone();
    lifecycle.enter(phase);
    if let Err(_error) = store
        .update_running_agent_execution_lifecycle(
            agent_execution_id,
            role_execution_id,
            &lifecycle.value(),
        )
        .await
    {
        lifecycle.restore_after_persistence_failure(&previous, phase);
        return Err(RoleLifecyclePersistenceFailed.into());
    }
    Ok(())
}

pub(super) async fn confirm_agent_lifecycle_phase(
    store: &WorkflowStore,
    lifecycle: &mut AcpRoleLifecycle,
    agent_execution_id: &str,
    role_execution_id: &str,
    phase: &'static str,
) -> Result<()> {
    let previous = lifecycle.clone();
    lifecycle.confirm(phase);
    if let Err(_error) = store
        .update_running_agent_execution_lifecycle(
            agent_execution_id,
            role_execution_id,
            &lifecycle.value(),
        )
        .await
    {
        lifecycle.restore_after_persistence_failure(&previous, phase);
        return Err(RoleLifecyclePersistenceFailed.into());
    }
    Ok(())
}

pub(super) fn classify_supervisor_evidence(
    status: std::process::ExitStatus,
    cleanup: Result<crate::acp_process::CleanupReceiptEvidence>,
) -> SupervisorEvidence {
    use std::os::unix::process::ExitStatusExt;
    let exit_code = status.code();
    let signal = status.signal();
    let mut failure = if signal.is_some() {
        Some("SUPERVISOR_SIGNALED")
    } else if !status.success() {
        Some("SUPERVISOR_NONZERO_EXIT")
    } else {
        None
    };
    let (receipt, cleanup_confirmed) = match cleanup {
        Ok(receipt)
            if receipt.runtime == "podman"
                && receipt.expected_image_matches
                && (exit_code == Some(receipt.exit_code) || signal.is_some()) =>
        {
            (Some(receipt), true)
        }
        Ok(receipt) => {
            failure.get_or_insert("CLEANUP_RECEIPT_MISMATCH");
            (Some(receipt), false)
        }
        Err(_) => {
            failure.get_or_insert("CLEANUP_RECEIPT_UNCONFIRMED");
            (None, false)
        }
    };
    SupervisorEvidence {
        exit_code,
        signal,
        cleanup_confirmed,
        failure,
        receipt,
    }
}

#[cfg(test)]
pub(super) fn validate_role_turn_completion(
    output: &str,
    evidence: &SupervisorEvidence,
) -> Result<()> {
    if let Some(reason) = evidence.failure {
        bail!("{reason}");
    }
    ensure!(evidence.cleanup_confirmed, "CLEANUP_RECEIPT_UNCONFIRMED");
    ensure!(
        output.contains(ORBIT_HANDOFF_START) && output.contains(ORBIT_HANDOFF_END),
        "HANDOFF_PARSE_FAILED"
    );
    Ok(())
}

pub(super) async fn wait_cli_supervisor(
    child: &mut tokio::process::Child,
    wait_timeout: Duration,
    kill_timeout: Duration,
) -> Result<std::process::ExitStatus> {
    match tokio::time::timeout(wait_timeout, child.wait()).await {
        Ok(Ok(status)) => Ok(status),
        Ok(Err(error)) => {
            Err(UnconfirmedRoleCleanup(format!("supervisor wait failed: {error}")).into())
        }
        Err(_) => {
            if let Some(pid) = child.id() {
                unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            }
            child.start_kill().map_err(|error| {
                UnconfirmedRoleCleanup(format!("kill timed-out supervisor failed: {error}"))
            })?;
            tokio::time::timeout(kill_timeout, child.wait())
                .await
                .map_err(|_| UnconfirmedRoleCleanup("supervisor did not exit after kill".into()))?
                .map_err(|error| UnconfirmedRoleCleanup(format!("supervisor reap failed: {error}")))
                .map_err(Into::into)
        }
    }
}

pub(super) fn record_supervisor_evidence(
    lifecycle: &mut AcpRoleLifecycle,
    evidence: &SupervisorEvidence,
) {
    lifecycle.process_exit_code = evidence.exit_code;
    lifecycle.process_signal = evidence.signal;
    lifecycle.supervisor_outcome = if evidence.signal.is_some() {
        "SIGNALED"
    } else {
        match evidence.exit_code {
            Some(0) => "EXITED_ZERO",
            Some(_) => "EXITED_NONZERO",
            None => "EXIT_UNCONFIRMED",
        }
    };
    lifecycle.supervisor_failure = evidence.failure;
    lifecycle.cleanup_state = if evidence.cleanup_confirmed {
        "CONFIRMED"
    } else {
        "UNCONFIRMED"
    };
    lifecycle.supervisor_receipt = evidence.receipt.clone();
    if evidence.exit_code.is_some() || evidence.signal.is_some() {
        lifecycle.confirm("SUPERVISOR_EXIT_OBSERVED");
    }
    if evidence.cleanup_confirmed {
        lifecycle.confirm("CLEANUP_CONFIRMED");
    } else {
        lifecycle.phase = "CLEANUP_UNCONFIRMED";
    }
}

pub(super) fn preserve_primary_failure(
    primary: &mut Option<anyhow::Error>,
    secondary: anyhow::Error,
) {
    primary.get_or_insert(secondary);
}

pub(super) fn add_supervisor_failure_if_primary_missing(
    failure: &mut Option<anyhow::Error>,
    evidence: &SupervisorEvidence,
) {
    if failure.is_some() {
        return;
    }
    if let Some(reason) = evidence.failure {
        preserve_primary_failure(failure, RoleSupervisorOutcomeFailure(reason).into());
    } else if !evidence.cleanup_confirmed {
        preserve_primary_failure(
            failure,
            RoleSupervisorOutcomeFailure("CLEANUP_RECEIPT_UNCONFIRMED").into(),
        );
    }
}

pub(super) async fn terminate_supervisor_after_evidence_failure(
    child: &mut tokio::process::Child,
    request_path: &Path,
    attempt_id: &str,
    expected_image: &str,
) -> SupervisorEvidence {
    // Closing the worker lifeline asks the supervisor to stop its container
    // and write the cleanup receipt before escalation.
    drop(child.stdin.take());
    match wait_cli_supervisor(child, Duration::from_secs(5), Duration::from_secs(5)).await {
        Ok(status) => classify_supervisor_evidence(
            status,
            crate::acp_process::read_cleanup_evidence(
                request_path,
                Some(attempt_id),
                expected_image,
            ),
        ),
        Err(_) => {
            if let Some(pid) = child.id() {
                unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            }
            let _ = child.start_kill();
            let reap = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
            if let Ok(Ok(status)) = reap {
                return classify_supervisor_evidence(
                    status,
                    crate::acp_process::read_cleanup_evidence(
                        request_path,
                        Some(attempt_id),
                        expected_image,
                    ),
                );
            }
            let receipt = crate::acp_process::read_cleanup_evidence(
                request_path,
                Some(attempt_id),
                expected_image,
            )
            .ok();
            SupervisorEvidence {
                exit_code: None,
                signal: None,
                cleanup_confirmed: false,
                failure: Some("SUPERVISOR_EXIT_UNCONFIRMED"),
                receipt,
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_real_acp_turn(
    workflow_state_pool: &PgPool,
    credential_catalog_pool: &PgPool,
    wf_run: &WorkflowRun,
    role_exec: &RoleExecution,
    role: &RoleDefinition,
    target: &ResolvedExecutionTarget,
    task_text: &str,
    repo_path: &Path,
    input_handoff: Option<&HandoffArtifact>,
    cancellation: tokio::sync::watch::Receiver<bool>,
    suppress_diagnostics: bool,
) -> Result<RoleExecutionOutcome> {
    let agent_exec_id = format!("acp-exec-{}", id());
    let started_at_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as i64;
    let store = WorkflowStore::new(workflow_state_pool.clone());
    let mut lifecycle = AcpRoleLifecycle::new();
    let initial_audit = ToolCallAudit::default().metadata(0, 0, 0);
    let expected_runtime_profile =
        target
            .runtime_image_digest
            .clone()
            .or_else(|| match target.provider.as_str() {
                "codex" => Some(crate::codex_credential_enrollment::CODEX_IMAGE.to_string()),
                "antigravity" => Some(ANTIGRAVITY_IMAGE.to_string()),
                _ => None,
            });
    let initial_metadata = serde_json::json!({
        "provider": target.provider,
        "account_reference": target.credential_id,
        "credential_generation": target.credential_generation,
        "requested_model": target.requested_model,
        "resolved_model": target.resolved_model,
        "expected_runtime_identity": target.runtime_interface,
        "expected_runtime_profile": expected_runtime_profile,
        "cleanup_confirmed": false,
        "observed_model": null,
        "tool_call_audit": initial_audit,
        "lifecycle": lifecycle.value(),
    });
    store
        .start_agent_execution(
            &agent_exec_id,
            &role_exec.id,
            &format!("{}-acp", target.provider),
            Some(&target.provider),
            target.resolved_model.as_deref(),
            started_at_ms,
            target.requested_model.as_deref(),
            target.resolved_model.as_deref(),
            &initial_metadata,
        )
        .await
        .map_err(|_| anyhow::anyhow!("AGENT_EXECUTION_START_FAILED"))?;

    let result = execute_real_acp_turn_body(
        workflow_state_pool,
        credential_catalog_pool,
        wf_run,
        role_exec,
        role,
        target,
        task_text,
        repo_path,
        input_handoff,
        cancellation,
        suppress_diagnostics,
        &agent_exec_id,
        &mut lifecycle,
    )
    .await;

    if !lifecycle.needs_failure_finalization() {
        return result;
    }

    let normalized_reason = match &result {
        Err(error) => lifecycle.normalized_failure(error),
        Ok(_) => "ACP_EXECUTION_NOT_FINALIZED",
    };
    if let Err(error) = &result {
        lifecycle.record_error(error);
    } else {
        lifecycle.failed_phase = Some(lifecycle.phase);
        lifecycle.outcome = "FAILED";
        lifecycle.normalized_reason = Some(normalized_reason);
        lifecycle.phase = "TERMINAL";
    }
    lifecycle.preserve_cleanup_state_for_failure_finalization();
    let finished_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let failure_metadata = serde_json::json!({
        "cleanup_confirmed": lifecycle.cleanup_state == "CONFIRMED",
        "observed_model": null,
        "lifecycle": lifecycle.value(),
    });
    lifecycle.note_terminal_persistence_attempt();
    if store
        .finish_agent_execution(
            &agent_exec_id,
            &role_exec.id,
            finished_at_ms,
            "FAILED",
            Some(normalized_reason),
            lifecycle.process_exit_code,
            Some(normalized_reason),
            target.requested_model.as_deref(),
            target.resolved_model.as_deref(),
            None,
            0,
            0,
            0,
            0,
            &serde_json::json!({}),
            &failure_metadata,
        )
        .await
        .is_err()
    {
        lifecycle.note_terminal_persistence_failure();
        return Err(RoleTerminalPersistenceUnconfirmed.into());
    }
    Err(anyhow::anyhow!(normalized_reason))
}

#[allow(clippy::too_many_arguments)]
async fn execute_real_acp_turn_body(
    workflow_state_pool: &PgPool,
    credential_catalog_pool: &PgPool,
    wf_run: &WorkflowRun,
    role_exec: &RoleExecution,
    role: &RoleDefinition,
    target: &ResolvedExecutionTarget,
    task_text: &str,
    repo_path: &Path,
    input_handoff: Option<&HandoffArtifact>,
    mut cancellation: tokio::sync::watch::Receiver<bool>,
    suppress_diagnostics: bool,
    agent_exec_id: &str,
    lifecycle: &mut AcpRoleLifecycle,
) -> Result<RoleExecutionOutcome> {
    let store = WorkflowStore::new(workflow_state_pool.clone());

    persist_agent_lifecycle_phase(
        &store,
        lifecycle,
        agent_exec_id,
        &role_exec.id,
        "CREDENTIAL_RESOLUTION",
    )
    .await?;
    let cred_store = CredentialStore::new(credential_catalog_pool);
    let credential_ref = target
        .credential_id
        .as_deref()
        .context("CREDENTIAL_PIN_REQUIRED")?;
    let credential = cred_store
        .get(credential_ref)
        .await?
        .context("target credential not found")?;
    validate_role_credential_target(target, &credential)?;
    confirm_agent_lifecycle_phase(
        &store,
        lifecycle,
        agent_exec_id,
        &role_exec.id,
        "CREDENTIAL_RESOLVED",
    )
    .await?;

    persist_agent_lifecycle_phase(
        &store,
        lifecycle,
        agent_exec_id,
        &role_exec.id,
        "CREDENTIAL_STAGING",
    )
    .await?;
    let backend = LocalPrivateSecretBackend::default_for_operator()?;

    let scratch_dir = tempfile::Builder::new()
        .prefix("orbit-acp-role-")
        .tempdir()?;
    let auth_store_dir = scratch_dir.path().join("auth");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&auth_store_dir)?;
    let auth_store_dir = auth_store_dir.canonicalize()?;

    let runtime = if target.provider == "codex" {
        let secret_bytes =
            registered_auth_diagnostic(credential_catalog_pool, &backend, &credential.reference)
                .await
                .map_err(|e| {
                    anyhow::anyhow!(
                        "failed to stage Codex credentials for {}: {:?}",
                        credential.reference,
                        e
                    )
                })?;
        let auth_file = auth_store_dir.join("auth.json");
        tokio::fs::write(&auth_file, secret_bytes.expose()).await?;
        std::fs::set_permissions(&auth_file, std::fs::Permissions::from_mode(0o600))?;
        confirm_agent_lifecycle_phase(
            &store,
            lifecycle,
            agent_exec_id,
            &role_exec.id,
            "CREDENTIAL_STAGED",
        )
        .await?;
        persist_agent_lifecycle_phase(
            &store,
            lifecycle,
            agent_exec_id,
            &role_exec.id,
            "RUNTIME_PREPARATION",
        )
        .await?;

        use crate::codex_credential_enrollment as enrolled;
        let binding_name = "codex-role-v1";
        let launch = Launch {
            adapter: Adapter::Codex,
            image: enrolled::CODEX_IMAGE.into(),
            command: vec![enrolled::CODEX_BINARY.into(), "app-server".into()],
            agent_name: "orbit-codex-acp".into(),
            agent_version: "1".into(),
            binary_revision: enrolled::CODEX_VERSION.into(),
            cpu_millis: 1000,
            memory_mib: 512,
            network: AgentNetwork::Host,
        };
        let auth = Auth {
            source: "codex".into(),
            owner: credential.reference.clone(),
            account_class: "chatgpt".into(),
            mode: AuthMode::LocalSession,
        };
        let descriptor = Descriptor {
            agent_id: "codex".into(),
            agent_revision: crate::codex_bridge::REVISION.into(),
            launch_digest: launch.digest()?,
            protocol_version: 1,
            auth: auth.clone(),
            security_profile: SecurityProfile::Trusted,
            filesystem_policy: FilesystemPolicy::AttemptWorkspace,
            terminal_policy: TerminalPolicy::WorkspaceSupervisor,
            model_policy: ModelPolicy::Exact,
            accounting: Accounting::ExecutionOnly,
            max_limits: AcpLimits {
                prompt_turns: 1,
                broker_calls: crate::tools::budget::RoleBudget::for_role(&role.role_id)
                    .max_total_calls as u32,
                reported_tool_calls: crate::tools::budget::RoleBudget::for_role(&role.role_id)
                    .max_total_calls as u32,
                turn_timeout_seconds: 300,
                terminal_timeout_seconds: 30,
                terminal_runtime_seconds: 0,
                output_bytes: 524288,
            },
        };
        let mut files = BTreeMap::new();
        files.insert("auth.json".into(), enrolled::CODEX_AUTH_RELATIVE.into());
        let rt = Runtime {
            binding_name: binding_name.into(),
            binding: Binding {
                model: target
                    .resolved_model
                    .clone()
                    .or_else(|| Some("gpt-6-luna".into())),
                runtime: "agent.codex-role-v1".into(),
                tools: Default::default(),
                permissions: Vec::new(),
                max_budget: Budget {
                    tokens: None,
                    cost_microusd: None,
                    calls: 1,
                },
                max_delegations: 0,
                acp: Some(descriptor),
            },
            launch,
            auth: AuthStore {
                path: auth_store_dir.clone(),
                source: auth.source,
                owner: auth.owner,
                account_class: auth.account_class,
                files,
                scopes: Vec::new(),
            },
            reasoning_effort: None,
        };
        rt.validate()?;
        rt
    } else if target.provider == "antigravity" {
        let inspection = cred_store
            .inspect(&credential.reference)
            .await?
            .context("antigravity credential inspection missing")?;
        let view = inspection
            .representations
            .iter()
            .find(|r| {
                r.interface == "acp"
                    && r.generation == credential.generation
                    && r.current_generation
            })
            .context("antigravity acp representation missing")?;
        let representation = cred_store
            .representation(&view.id)
            .await?
            .context("antigravity acp representation entity missing")?;
        let locator = representation
            .secret_locator
            .context("missing secret locator for antigravity acp")?;
        let bundle = backend.read(locator).await?;
        let (token, settings) = decode_bundle(&bundle)?;

        let token_file = auth_store_dir.join("acp_token.json");
        tokio::fs::write(&token_file, token.expose()).await?;
        std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600))?;

        let settings_file = auth_store_dir.join("settings.json");
        tokio::fs::write(&settings_file, settings.expose()).await?;
        std::fs::set_permissions(&settings_file, std::fs::Permissions::from_mode(0o600))?;
        confirm_agent_lifecycle_phase(
            &store,
            lifecycle,
            agent_exec_id,
            &role_exec.id,
            "CREDENTIAL_STAGED",
        )
        .await?;
        persist_agent_lifecycle_phase(
            &store,
            lifecycle,
            agent_exec_id,
            &role_exec.id,
            "RUNTIME_PREPARATION",
        )
        .await?;

        let binding_name = "antigravity-role-v1";
        let launch = Launch {
            adapter: Adapter::Antigravity,
            image: target
                .runtime_image_digest
                .clone()
                .unwrap_or_else(|| ANTIGRAVITY_IMAGE.into()),
            command: vec![ACP_EXECUTABLE.into()],
            agent_name: "antigravity-acp".into(),
            agent_version: crate::acp_capabilities::ANTIGRAVITY_ACP_ADAPTER_REVISION.into(),
            binary_revision: "1.1.1".into(),
            cpu_millis: 1000,
            memory_mib: 512,
            network: AgentNetwork::Host,
        };
        let auth = Auth {
            source: "antigravity".into(),
            owner: credential.reference.clone(),
            account_class: "personal".into(),
            mode: AuthMode::LocalSession,
        };
        let descriptor = Descriptor {
            agent_id: "antigravity-acp".into(),
            agent_revision: "1.1.1".into(),
            launch_digest: launch.digest()?,
            protocol_version: 1,
            auth: auth.clone(),
            security_profile: SecurityProfile::Trusted,
            filesystem_policy: FilesystemPolicy::AttemptWorkspace,
            terminal_policy: TerminalPolicy::WorkspaceSupervisor,
            model_policy: ModelPolicy::Exact,
            accounting: Accounting::ExecutionOnly,
            max_limits: AcpLimits {
                prompt_turns: 1,
                broker_calls: crate::tools::budget::RoleBudget::for_role(&role.role_id)
                    .max_total_calls as u32,
                reported_tool_calls: crate::tools::budget::RoleBudget::for_role(&role.role_id)
                    .max_total_calls as u32,
                turn_timeout_seconds: 300,
                terminal_timeout_seconds: 30,
                terminal_runtime_seconds: 0,
                output_bytes: 524288,
            },
        };
        let mut files = BTreeMap::new();
        files.insert(
            "acp_token.json".into(),
            ".gemini/antigravity-acp/acp_token.json".into(),
        );
        files.insert(
            "settings.json".into(),
            ".gemini/antigravity-acp/settings.json".into(),
        );
        let rt = Runtime {
            binding_name: binding_name.into(),
            binding: Binding {
                model: target
                    .resolved_model
                    .clone()
                    .or_else(|| Some("gemini-3.8-flash".into())),
                runtime: "agent.antigravity-role-v1".into(),
                tools: Default::default(),
                permissions: Vec::new(),
                max_budget: Budget {
                    tokens: None,
                    cost_microusd: None,
                    calls: 1,
                },
                max_delegations: 0,
                acp: Some(descriptor),
            },
            launch,
            auth: AuthStore {
                path: auth_store_dir.clone(),
                source: auth.source,
                owner: auth.owner,
                account_class: auth.account_class,
                files,
                scopes: Vec::new(),
            },
            reasoning_effort: None,
        };
        rt.validate()?;
        rt
    } else {
        bail!("unsupported role provider: {}", target.provider);
    };

    let execution_profile = store.execution_profile(&wf_run.id).await?;
    let role_budget = crate::tools::budget::RoleBudget::for_role(&role.role_id);
    role_budget.validate()?;
    let mut allowed_tools = repository_tools_for_role(role);
    if matches!(
        execution_profile,
        crate::execution::local::RoleExecutionProfile::DevLocal { .. }
    ) && role.workspace_access == WorkspaceAccess::ReadWrite
    {
        allowed_tools.push("shell".into());
    }

    let request_path = scratch_dir.path().join("request.json");
    let current_credential = cred_store
        .get(credential_ref)
        .await?
        .context("target credential disappeared before execution")?;
    validate_role_credential_target(target, &current_credential)?;
    let req = crate::acp_process::Request {
        runtime,
        attempt_id: id(),
        timeout_seconds: 600,
        tools: allowed_tools.clone(),
    };
    tokio::fs::write(&request_path, serde_json::to_vec(&req)?).await?;
    std::fs::set_permissions(&request_path, std::fs::Permissions::from_mode(0o600))?;
    confirm_agent_lifecycle_phase(
        &store,
        lifecycle,
        agent_exec_id,
        &role_exec.id,
        "RUNTIME_PREPARED",
    )
    .await?;

    persist_agent_lifecycle_phase(
        &store,
        lifecycle,
        agent_exec_id,
        &role_exec.id,
        "SUPERVISOR_START",
    )
    .await?;
    let mut command = tokio::process::Command::new(crate::worker::current_executable()?);
    command
        .arg("acp-supervisor")
        .arg("--request")
        .arg(&request_path)
        .current_dir(scratch_dir.path())
        .env_clear()
        .env(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
        )
        .env(
            "HOME",
            std::env::var_os("HOME").context("rootless runtime HOME missing")?,
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(if suppress_diagnostics {
            Stdio::null()
        } else {
            Stdio::inherit()
        })
        .kill_on_drop(true);
    command.process_group(0);

    if let Some(val) = std::env::var_os("XDG_RUNTIME_DIR") {
        command.env("XDG_RUNTIME_DIR", val);
    }

    let mut child = command
        .spawn()
        .map_err(|_| anyhow::anyhow!("SUPERVISOR_START_FAILED"))?;
    lifecycle.cleanup_state = "UNCONFIRMED";
    if let Err(error) = confirm_agent_lifecycle_phase(
        &store,
        lifecycle,
        agent_exec_id,
        &role_exec.id,
        "SUPERVISOR_STARTED",
    )
    .await
    {
        let evidence = terminate_supervisor_after_evidence_failure(
            &mut child,
            &request_path,
            &req.attempt_id,
            &req.runtime.launch.image,
        )
        .await;
        record_supervisor_evidence(lifecycle, &evidence);
        return Err(error);
    }

    let Some(child_out) = child.stdout.take() else {
        let evidence = terminate_supervisor_after_evidence_failure(
            &mut child,
            &request_path,
            &req.attempt_id,
            &req.runtime.launch.image,
        )
        .await;
        record_supervisor_evidence(lifecycle, &evidence);
        return Err(anyhow::anyhow!("SUPERVISOR_STDOUT_UNAVAILABLE"));
    };
    let Some(child_in) = child.stdin.take() else {
        let evidence = terminate_supervisor_after_evidence_failure(
            &mut child,
            &request_path,
            &req.attempt_id,
            &req.runtime.launch.image,
        )
        .await;
        record_supervisor_evidence(lifecycle, &evidence);
        return Err(anyhow::anyhow!("SUPERVISOR_STDIN_UNAVAILABLE"));
    };
    let mut wire = Wire::new(child_out, child_in, 16 * 1024 * 1024);

    let mut state = AcpTurnState {
        repo_path,
        workspace_access: role.workspace_access,
        role_id: Some(role.role_id.clone()),
        workspace_identity: wf_run.repository_path.clone(),
        tool_call_limit: role_budget.max_total_calls,
        role_budget: Some(role_budget.clone()),
        role_usage: Default::default(),
        execution_profile,
        agent_output: String::new(),
        tool_calls: 0,
        tool_successes: 0,
        tool_failures: 0,
        tool_counts: BTreeMap::new(),
        tool_call_audit: ToolCallAudit::with_context(Some(&role.role_id), Some(&allowed_tools)),
        terminals: BTreeMap::new(),
        wf_attempt_id: Some(wf_run.attempt_id.clone()),
        role_exec_id: Some(role_exec.id.clone()),
        agent_exec_id: Some(agent_exec_id.to_string()),
        pool: Some(workflow_state_pool),
    };

    let terminal_enabled = matches!(
        state.execution_profile,
        crate::execution::local::RoleExecutionProfile::DevLocal { .. }
    ) && role.workspace_access == WorkspaceAccess::ReadWrite;
    let turn = tokio::select! {
        result = async {
    persist_agent_lifecycle_phase(
        &store,
        lifecycle,
        agent_exec_id,
        &role_exec.id,
        "ACP_INITIALIZE",
    )
    .await?;
    let _init_res = acp_call(
        &mut wire,
        &mut state,
        "initialize",
        serde_json::json!({
            "protocolVersion": 1,
            "clientInfo": {
                "name": "orbit",
                "version": env!("CARGO_PKG_VERSION")
            },
            "clientCapabilities": {
                "fs": {
                    "readTextFile": true,
                    "writeTextFile": role.workspace_access == WorkspaceAccess::ReadWrite,
                    "listDirectory": true,
                    "findPath": true,
                    "editFile": role.workspace_access == WorkspaceAccess::ReadWrite,
                    "copy": role.workspace_access == WorkspaceAccess::ReadWrite,
                    "createDirectory": role.workspace_access == WorkspaceAccess::ReadWrite,
                    "move": role.workspace_access == WorkspaceAccess::ReadWrite,
                    "deleteFile": role.workspace_access == WorkspaceAccess::ReadWrite,
                    "deleteDirectory": role.workspace_access == WorkspaceAccess::ReadWrite
                },
                "search": {
                    "grep": true
                },
                "git": {
                    "status": true,
                    "diff": true,
                    "show": true
                },
                "_meta": {"orbit": {"atomicShell": terminal_enabled, "byteReads": true}},
                "terminal": terminal_enabled
            }
        }),
    )
    .await
    .context("ACP initialize failed")?;

    confirm_agent_lifecycle_phase(
        &store,
        lifecycle,
        agent_exec_id,
        &role_exec.id,
        "ACP_INITIALIZED",
    )
    .await?;
    persist_agent_lifecycle_phase(
        &store,
        lifecycle,
        agent_exec_id,
        &role_exec.id,
        "SESSION_CREATION",
    )
    .await?;
    let new_res = acp_call(
        &mut wire,
        &mut state,
        "session/new",
        serde_json::json!({
            "cwd": crate::acp_runtime::WORKSPACE,
            "mcpServers": []
        }),
    )
    .await
    .context("ACP session/new failed")?;

    let session_id = new_res
        .get("sessionId")
        .and_then(|v| v.as_str())
        .context("sessionId missing in session/new response")?
        .to_string();
    confirm_agent_lifecycle_phase(
        &store,
        lifecycle,
        agent_exec_id,
        &role_exec.id,
        "SESSION_CREATED",
    )
    .await?;

    if target.provider == "antigravity" {
        let _ = acp_call(
            &mut wire,
            &mut state,
            "session/set_mode",
            serde_json::json!({
                "sessionId": session_id,
                "modeId": "yolo"
            }),
        )
        .await;
    }

    let git_diff = if role.role_id == "reviewer" {
        Some(
            review_candidate_diff(repo_path, wf_run.base_revision.as_deref().unwrap_or("HEAD"))
                .await?,
        )
    } else {
        None
    };

    let base_rev = wf_run.base_revision.as_deref().unwrap_or("HEAD");
    let available_verification_check_ids = if role.role_id == "reviewer" {
        match (
            wf_run.selection_policy_id.as_deref(),
            wf_run.selection_policy_version,
            wf_run.selection_policy_digest.as_deref(),
        ) {
            (Some(id), Some(version), Some(expected_digest)) => {
                let policy = crate::regression_strategy::RegressionStore::new(
                    workflow_state_pool.clone(),
                )
                .get_selection_policy(id, version)
                .await?
                .context("reviewer selection policy is missing")?;
                ensure!(
                    policy.digest() == expected_digest,
                    "POLICY_DIGEST_MISMATCH: reviewer selection policy differs from workflow pin"
                );
                policy
                    .checks
                    .iter()
                    .map(|check| check.check_id.clone())
                    .collect::<Vec<_>>()
            }
            (None, None, None) => Vec::new(),
            _ => bail!("INCOMPLETE_POLICY_PIN: reviewer selection policy reference is incomplete"),
        }
    } else {
        Vec::new()
    };
    let prompt_text = build_role_prompt(
        RolePromptToolContext {
            provider: &target.provider,
            role,
            advertised_tools: &allowed_tools,
        },
        task_text,
        repo_path,
        base_rev,
        input_handoff,
        git_diff.as_deref(),
        &available_verification_check_ids,
    )?;

    persist_agent_lifecycle_phase(
        &store,
        lifecycle,
        agent_exec_id,
        &role_exec.id,
        "PROMPT_DISPATCH",
    )
    .await?;
    lifecycle.prompt_uncertainty = "IN_FLIGHT";
    persist_agent_lifecycle_phase(
        &store,
        lifecycle,
        agent_exec_id,
        &role_exec.id,
        "PROMPT_IN_FLIGHT",
    )
    .await?;
    let prompt_res = acp_call(
        &mut wire,
        &mut state,
        "session/prompt",
        serde_json::json!({
            "sessionId": session_id,
            "prompt": [
                {
                    "type": "text",
                    "text": prompt_text
                }
            ]
        }),
    )
    .await
    .context("ACP session/prompt failed")?;
    lifecycle.prompt_uncertainty = "RESOLVED";
    confirm_agent_lifecycle_phase(
        &store,
        lifecycle,
        agent_exec_id,
        &role_exec.id,
        "PROMPT_RESPONSE_RECEIVED",
    )
    .await?;

    if !state.agent_output.contains(ORBIT_HANDOFF_START) {
        extract_text_from_json(&prompt_res, &mut state.agent_output);
    }
    Ok::<(), anyhow::Error>(())
        } => result,
        _ = async {
            if *cancellation.borrow() { return; }
            while cancellation.changed().await.is_ok() {
                if *cancellation.borrow() { return; }
            }
            std::future::pending::<()>().await;
        } => Err(anyhow::Error::new(RoleExecutionCancelled)),
        _ = tokio::time::sleep(Duration::from_secs(600)) => Err(anyhow::Error::new(RoleSupervisorTimeout)),
    };

    let failure_phase = lifecycle.phase;
    let cleanup_phase_result =
        persist_agent_lifecycle_phase(&store, lifecycle, agent_exec_id, &role_exec.id, "CLEANUP")
            .await;
    drop(wire);
    let wait_limit = if turn.is_err() { 5 } else { 60 };
    let status = wait_cli_supervisor(
        &mut child,
        Duration::from_secs(wait_limit),
        Duration::from_secs(10),
    )
    .await;
    let evidence = match status {
        Ok(status) => classify_supervisor_evidence(
            status,
            crate::acp_process::read_cleanup_evidence(
                &request_path,
                Some(&req.attempt_id),
                &req.runtime.launch.image,
            ),
        ),
        Err(_) => {
            terminate_supervisor_after_evidence_failure(
                &mut child,
                &request_path,
                &req.attempt_id,
                &req.runtime.launch.image,
            )
            .await
        }
    };
    record_supervisor_evidence(lifecycle, &evidence);
    if state.tool_calls > 0
        || !state.tool_call_audit.provider_tool_invocations.is_empty()
        || !state.tool_call_audit.unmatched_provider_updates.is_empty()
        || state.tool_call_audit.provider_tool_names_omitted > 0
    {
        lifecycle.note_tool_activity();
    }
    let mut terminal_cleanup_failed = false;
    for (_tid, term) in std::mem::take(&mut state.terminals) {
        if term.kill().await.is_err() {
            terminal_cleanup_failed = true;
        }
    }

    let finished_at_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as i64;

    let mut failure = turn.err();
    if let Err(error) = cleanup_phase_result {
        preserve_primary_failure(&mut failure, error);
    }
    if terminal_cleanup_failed {
        preserve_primary_failure(&mut failure, RoleTerminalCleanupFailed.into());
        lifecycle.cleanup_state = "UNCONFIRMED";
    }
    add_supervisor_failure_if_primary_missing(&mut failure, &evidence);
    if failure.is_none()
        && (!state.agent_output.contains(ORBIT_HANDOFF_START)
            || !state.agent_output.contains(ORBIT_HANDOFF_END))
    {
        preserve_primary_failure(&mut failure, RoleHandoffResponseMissing.into());
    }
    if let Some(error) = failure.as_ref() {
        lifecycle.phase = failure_phase;
        lifecycle.record_error(error);
    } else {
        lifecycle.finish_success();
    }
    lifecycle.cleanup_state = if evidence.cleanup_confirmed {
        if terminal_cleanup_failed {
            "UNCONFIRMED"
        } else {
            "CONFIRMED"
        }
    } else {
        "UNCONFIRMED"
    };
    lifecycle.process_exit_code = evidence.exit_code;
    lifecycle.process_signal = evidence.signal;
    lifecycle.supervisor_receipt = evidence.receipt.clone();
    lifecycle.phase = "TERMINAL";
    state.tool_call_audit.finish_interrupted_call(
        &mut state.tool_successes,
        &mut state.tool_failures,
        failure.as_ref(),
    );
    state
        .tool_call_audit
        .set_turn_completion(failure.is_none(), state.tool_calls);
    let status_text = if failure.is_some() {
        "FAILED"
    } else {
        "SUCCEEDED"
    };
    let reason = lifecycle.normalized_reason.unwrap_or("COMPLETED");
    let failure_message = failure.as_ref().map(|_| reason.to_owned());
    let model_evidence = role_model_evidence(target, None);
    let tool_call_audit =
        state
            .tool_call_audit
            .metadata(state.tool_calls, state.tool_successes, state.tool_failures);
    let tool_counts = serde_json::to_value(&state.tool_counts).map_err(|_| {
        lifecycle.note_terminal_persistence_failure();
        anyhow::Error::new(RoleTerminalPersistenceUnconfirmed)
    })?;
    lifecycle.note_terminal_persistence_attempt();
    if store
        .finish_agent_execution(
            agent_exec_id,
            &role_exec.id,
            finished_at_ms,
            status_text,
            Some(reason),
            evidence.exit_code,
            failure_message.as_deref(),
            model_evidence.requested,
            model_evidence.configured,
            model_evidence.observed,
            i64::from(lifecycle.prompt_uncertainty != "NOT_DISPATCHED"),
            state.tool_calls as i64,
            state.tool_successes as i64,
            state.tool_failures as i64,
            &tool_counts,
            &serde_json::json!({
                "provider": target.provider,
                "execution_profile": state.execution_profile,
                "role_budget": {"limits":state.role_budget,"usage":state.role_usage},
                "cleanup_confirmed": evidence.cleanup_confirmed,
                "observed_model": null,
                "tool_call_audit": tool_call_audit,
                "lifecycle": lifecycle.value(),
            }),
        )
        .await
        .is_err()
    {
        lifecycle.note_terminal_persistence_failure();
        return Err(RoleTerminalPersistenceUnconfirmed.into());
    }
    lifecycle.note_terminal_persistence_attempt();

    if !evidence.cleanup_confirmed {
        return Err(anyhow::anyhow!(
            lifecycle
                .normalized_reason
                .unwrap_or("CLEANUP_RECEIPT_UNCONFIRMED")
        ));
    }

    if failure.is_some() {
        return Err(anyhow::anyhow!(reason));
    }

    Ok(RoleExecutionOutcome {
        raw_output: state.agent_output,
        agent_execution_ids: vec![agent_exec_id.to_string()],
        termination_reason: Some("completed".into()),
    })
}

/// Production ACP role agent executor that executes real ACP agent turns.
pub struct RealAcpRoleExecutor;

impl RealAcpRoleExecutor {
    /// Execute a real ACP role turn with separate workflow and credential stores.
    /// Production callers normally use the same pool for both stores.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_role_with_credential_catalog(
        &self,
        workflow_state_pool: &PgPool,
        credential_catalog_pool: &PgPool,
        wf_run: &WorkflowRun,
        role_exec: &RoleExecution,
        role: &RoleDefinition,
        target: &ResolvedExecutionTarget,
        task_text: &str,
        repo_path: &Path,
        input_handoff: Option<&HandoffArtifact>,
        cancellation: tokio::sync::watch::Receiver<bool>,
    ) -> Result<RoleExecutionOutcome> {
        execute_real_acp_turn(
            workflow_state_pool,
            credential_catalog_pool,
            wf_run,
            role_exec,
            role,
            target,
            task_text,
            repo_path,
            input_handoff,
            cancellation,
            true,
        )
        .await
    }
}

#[async_trait::async_trait]
impl RoleAgentExecutor for RealAcpRoleExecutor {
    async fn execute_role(
        &self,
        pool: &PgPool,
        wf_run: &WorkflowRun,
        role_exec: &RoleExecution,
        role: &RoleDefinition,
        target: &ResolvedExecutionTarget,
        task_text: &str,
        repo_path: &Path,
        input_handoff: Option<&HandoffArtifact>,
        cancellation: tokio::sync::watch::Receiver<bool>,
    ) -> Result<RoleExecutionOutcome> {
        execute_real_acp_turn(
            pool,
            pool,
            wf_run,
            role_exec,
            role,
            target,
            task_text,
            repo_path,
            input_handoff,
            cancellation,
            false,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrong_credential_generation_is_rejected() {
        let target = ResolvedExecutionTarget {
            provider: "codex".into(),
            runtime_interface: "codex-acp".into(),
            credential_id: Some("codex-main".into()),
            credential_generation: Some(3),
            requested_model: Some("requested".into()),
            resolved_model: Some("configured".into()),
            runtime_image_digest: None,
            resolution_reason: "test".into(),
        };
        let credential = crate::credential_registry::Credential {
            id: "fixture".into(),
            provider: "codex".into(),
            reference: "codex-main".into(),
            generation: 4,
            endpoint: None,
            auth_type: "local-session".into(),
            secret_backend: "local-private".into(),
            secret_locator: None,
            status: crate::credential_registry::CredentialStatus::Enrolled,
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        let provenance = role_model_evidence(&target, None);
        assert_eq!(provenance.requested, Some("requested"));
        assert_eq!(provenance.configured, Some("configured"));
        assert_eq!(provenance.observed, None);
        assert!(
            validate_role_credential_target(&target, &credential)
                .unwrap_err()
                .to_string()
                .contains("CREDENTIAL_GENERATION_CHANGED")
        );
        let same = crate::credential_registry::Credential {
            generation: 3,
            ..credential
        };
        assert!(validate_role_credential_target(&target, &same).is_ok());
    }
}
