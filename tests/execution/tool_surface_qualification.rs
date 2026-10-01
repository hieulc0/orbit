//! Phase B3.4 Qualification Test Suite: Core Coding Agent Tool Surface.
//!
//! Validates:
//! - Canonical identity resolution for all 18 tool names.
//! - Alias routing from ACP method names (fs/*, terminal/*, search/*, git/*).
//! - Provider alias routing (read_file, write_file, edit_file, shell, grep, etc.).
//! - Bridge alias routing (orbit_* variants).
//! - Unsupported tool rejection with ERR_UNSUPPORTED_TOOL or ACP -32601.
//! - Role matrix: Planner denied mutating tools (write, edit, create_dir, move, copy, delete_file, delete_dir, terminal/create).
//! - Role matrix: Implementer allowed fs, search, terminal, and git tools.
//! - Role matrix: Reviewer allowed read-only inspection tools, denied all mutating tools.
//! - Mutation lock enforcement: mutating tool denied when lock not held, succeeds when lock held.
//! - Path confinement: path outside workspace rejected (absolute path, traversal, symlink escape).
//! - fs/list_directory: flat listing, recursive listing, hidden file exclusion, max entries cap.
//! - fs/find_path: name glob matching, path filtering, result limit cap.
//! - search/grep: literal query match, regex query match, case sensitivity, max matches cap, line number reporting.
//! - fs/edit_file: exact match replacement, single match requirement when replace_all=false, ERR_NO_MATCH, ERR_MULTIPLE_MATCHES, replace_all=true.
//! - fs/copy: single file copy, recursive directory copy, collision failure when overwrite not allowed.
//! - git/status: untracked, modified, staged files correctly identified.
//! - git/diff: working tree diff, stat-only diff, base revision diff.
//! - git/show: commit show, commit stat show, file content show at revision.
//! - Terminal lifecycle: exit 0 with captured stdout.
//! - Terminal failure lifecycle: exit non-zero with captured stderr.
//! - Terminal kill lifecycle: running process killed, exit status captured, output available.
//! - Terminal release lifecycle: release closes handles and stops background capture.
//! - Terminal output preview truncation: large output capped to preview bound (<= 64 KiB) with truncated=true.
//! - Real Codex implementer fixture: real tool execution, repo inspection, targeted edits, git inspection, telemetry.
//! - Real Antigravity reviewer fixture: real review execution using inspection tools over modified workspace.

use anyhow::{Context, Result, ensure};
use orbit::{
    acp_wire::Wire,
    coding_agent,
    fs_tools::*,
    model::id,
    tool_surface::{
        AgentTerminal, CanonicalToolName, ERR_MULTIPLE_MATCHES, ERR_MUTATION_LOCK_REQUIRED,
        ERR_NO_MATCH, ERR_READ_ONLY_ROLE, ERR_UNSUPPORTED_TOOL, ToolMetadata, copy_path, edit_file,
        find_path, git_diff, git_show, git_status, list_directory, search_grep,
    },
    workflow::*,
    workflow_coordinator::*,
};
use serde_json::json;
use sqlx::PgPool;
#[cfg(feature = "fault-injection")]
use sqlx::Row;
#[cfg(feature = "fault-injection")]
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;
use std::{collections::BTreeMap, ops::Deref};
use std::{fs, path::Path};
#[cfg(feature = "fault-injection")]
use std::{
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf,
    str::FromStr,
};
use tempfile::tempdir;
#[cfg(feature = "fault-injection")]
use zeroize::Zeroizing;

struct TestContext {
    database: common::DisposablePgTestContext,
    store: WorkflowStore,
}

#[allow(dead_code)] // shared test helpers are used by different qualification binaries
#[path = "../common/mod.rs"]
mod common;

impl Deref for TestContext {
    type Target = common::DisposablePgTestContext;

    fn deref(&self) -> &Self::Target {
        &self.database
    }
}

async fn setup_test() -> Result<TestContext> {
    let database = common::DisposablePgTestContext::create("b34", 3).await?;
    let store = WorkflowStore::new(database.engine.pool.clone());
    Ok(TestContext { database, store })
}

async fn start_fixture_agent_execution(
    ctx: &TestContext,
    role_execution: &RoleExecution,
) -> Result<(String, ResolvedExecutionTarget)> {
    let target = ResolvedExecutionTarget {
        provider: "fixture".into(),
        runtime_interface: "fixture-callback".into(),
        credential_id: Some("synthetic-fixture-account".into()),
        credential_generation: Some(1),
        requested_model: Some("fixture-model".into()),
        resolved_model: Some("fixture-model".into()),
        runtime_image_digest: None,
        resolution_reason: "deterministic callback fixture".into(),
    };
    ctx.store
        .set_role_execution_resolved(&role_execution.id, &target)
        .await?;

    let running_role = ctx
        .store
        .get_role_execution(&role_execution.id)
        .await?
        .context("fixture RoleExecution was not persisted")?;
    ensure!(
        running_role.status == RoleExecutionStatus::Running,
        "fixture RoleExecution did not enter RUNNING after target resolution"
    );

    let agent_execution_id = format!("fixture-exec-{}", id());
    let started_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as i64;
    let initial_metadata = json!({
        "provider": target.provider,
        "account_reference": target.credential_id,
        "credential_generation": target.credential_generation,
        "requested_model": target.requested_model,
        "resolved_model": target.resolved_model,
        "expected_runtime_identity": target.runtime_interface,
        "observed_model": null,
        "cleanup_confirmed": false,
        "tool_call_audit": {
            "summary": {
                "total": 0,
                "successful": 0,
                "unsuccessful": 0,
                "mutating": 0,
                "mutating_unknown": 0,
                "denied": 0,
                "unmatched_provider_calls": 0,
                "unmatched_callbacks": 0
            },
            "entries": [],
            "omitted_count": 0,
            "provider_tool_names_omitted": 0
        }
    });
    ctx.store
        .start_agent_execution(
            &agent_execution_id,
            &role_execution.id,
            "fixture-callback",
            Some(&target.provider),
            target.resolved_model.as_deref(),
            started_at_ms,
            target.requested_model.as_deref(),
            target.resolved_model.as_deref(),
            &initial_metadata,
        )
        .await?;

    let linked_role = ctx
        .store
        .get_role_execution(&role_execution.id)
        .await?
        .context("fixture RoleExecution disappeared after AgentExecution start")?;
    ensure!(
        linked_role
            .agent_execution_ids
            .contains(&agent_execution_id),
        "fixture AgentExecution was not linked to its RoleExecution"
    );
    Ok((agent_execution_id, target))
}

async fn finish_fixture_agent_execution(
    ctx: &TestContext,
    role_execution: &RoleExecution,
    agent_execution_id: &str,
    target: &ResolvedExecutionTarget,
    state: &AcpTurnState<'_>,
) -> Result<()> {
    let finished_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as i64;
    ctx.store
        .finish_agent_execution(
            agent_execution_id,
            &role_execution.id,
            finished_at_ms,
            "SUCCEEDED",
            Some("CALLBACK_FIXTURE_COMPLETED"),
            None,
            Some("deterministic callback fixture completed"),
            target.requested_model.as_deref(),
            target.resolved_model.as_deref(),
            None,
            1,
            state.tool_calls as i64,
            state.tool_successes as i64,
            state.tool_failures as i64,
            &serde_json::to_value(&state.tool_counts)?,
            &json!({
                "cleanup_confirmed": true,
                "lifecycle": {
                    "phase": "TERMINAL",
                    "outcome": "SUCCEEDED",
                    "normalized_reason": "CALLBACK_FIXTURE_COMPLETED",
                    "cleanup_state": "NO_RUNTIME_RESOURCE_CREATED",
                    "tool_audit_applicability": "TOOL_PHASE_REACHED"
                }
            }),
        )
        .await
}

async fn dispatch_correlated_fixture_callback(
    server_wire: &mut Wire,
    client_wire: &mut Wire,
    state: &mut AcpTurnState<'_>,
    callback: serde_json::Value,
) -> Result<serde_json::Value> {
    let callback_id = callback
        .get("id")
        .and_then(serde_json::Value::as_u64)
        .context("fixture callback id must be an unsigned integer")?;
    let method = callback
        .get("method")
        .and_then(serde_json::Value::as_str)
        .context("fixture callback method is missing")?;
    let tool_kind = if method.starts_with("terminal/") {
        "execute"
    } else if matches!(
        method,
        "fs/write_text_file"
            | "fs/edit_file"
            | "fs/create_directory"
            | "fs/move"
            | "fs/copy"
            | "fs/delete_file"
            | "fs/delete_directory"
    ) {
        "edit"
    } else {
        "read"
    };
    let invocation = orbit::acp_wire::OrbitToolInvocationMeta::new(
        &format!("oti-fixture-{callback_id}"),
        &format!("provider-call-fixture-{callback_id}"),
    )?;
    handle_acp_message(
        server_wire,
        state,
        json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": invocation.provider_tool_call_id,
                    "title": method,
                    "kind": tool_kind,
                    "status": "in_progress"
                }
            },
            "_meta": invocation.envelope_metadata()
        }),
    )
    .await?;

    let mut callback = callback;
    callback["_meta"] = invocation.envelope_metadata();
    handle_acp_message(server_wire, state, callback).await?;
    client_wire.read().await
}

#[cfg(feature = "fault-injection")]
const LIVE_PROVIDER_OPT_IN: &str = "I_AUTHORIZE_LIVE_PROVIDER_CALLS";
#[cfg(feature = "fault-injection")]
const LIVE_PROVIDER_OPT_IN_ENV: &str = "ORBIT_B34_LIVE_PROVIDER_OPT_IN";
#[cfg(feature = "fault-injection")]
const LIVE_CREDENTIAL_URL_FILE_ENV: &str = "ORBIT_B34_LIVE_CREDENTIAL_DATABASE_URL_FILE";

#[cfg(feature = "fault-injection")]
async fn explicitly_authorized_live_credential_catalog() -> Result<PgPool> {
    ensure!(
        std::env::var(LIVE_PROVIDER_OPT_IN_ENV).as_deref() == Ok(LIVE_PROVIDER_OPT_IN),
        "live provider fixture requires explicit opt-in"
    );
    let path = PathBuf::from(
        std::env::var_os(LIVE_CREDENTIAL_URL_FILE_ENV)
            .context("live provider fixture requires an explicit credential URL file path")?,
    );
    ensure!(
        path.is_absolute() && path.canonicalize().ok().as_deref() == Some(path.as_path()),
        "credential URL file path must be absolute and canonical"
    );

    let operator_home = orbit::secret_backend::operator_home()?;
    let private_root = operator_home.join(".orbit/private");
    ensure!(
        private_root.canonicalize().ok().as_deref() == Some(private_root.as_path())
            && path.starts_with(&private_root),
        "credential URL file must be inside Orbit's private root"
    );

    let mut directory = path
        .parent()
        .context("credential URL file parent directory is unavailable")?;
    loop {
        let metadata = fs::symlink_metadata(directory)
            .map_err(|_| anyhow::anyhow!("credential URL directory is unavailable"))?;
        ensure!(
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o7777 == 0o700,
            "credential URL directory must be private and owned by the current user"
        );
        if directory == private_root {
            break;
        }
        directory = directory
            .parent()
            .filter(|parent| parent.starts_with(&private_root))
            .context("credential URL file is outside Orbit's private root")?;
    }

    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|_| anyhow::anyhow!("credential URL file is unavailable"))?;
    let metadata = file
        .metadata()
        .map_err(|_| anyhow::anyhow!("credential URL file metadata is unavailable"))?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0
            && metadata.mode() & 0o400 != 0
            && metadata.len() <= 8192,
        "credential URL file must be a private regular file of at most 8 KiB"
    );
    let mut url = Zeroizing::new(String::new());
    file.take(8193)
        .read_to_string(&mut url)
        .map_err(|_| anyhow::anyhow!("credential URL file contents are invalid"))?;
    let url = url.trim();
    ensure!(!url.is_empty(), "credential URL file is empty");

    let options = sqlx::postgres::PgConnectOptions::from_str(url)
        .map_err(|_| anyhow::anyhow!("credential catalog URL is invalid"))?;
    ensure!(
        matches!(options.get_host(), "127.0.0.1" | "::1" | "localhost")
            && options.get_port() == 55442
            && options.get_database() == Some("orbit_control_plane")
            && options.get_socket().is_none(),
        "credential catalog must target loopback:55442/orbit_control_plane"
    );

    PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options.options([("default_transaction_read_only", "on")]))
        .await
        .map_err(|_| {
            anyhow::anyhow!("unable to connect to explicitly authorized credential catalog")
        })
}

#[cfg(feature = "fault-injection")]
async fn ensure_live_catalog_is_separate(
    catalog_pool: &PgPool,
    workflow_pool: &PgPool,
    workflow_schema: &str,
) -> Result<()> {
    let catalog_identity: (String, String) =
        sqlx::query_as("SELECT current_database(), current_schema()")
            .fetch_one(catalog_pool)
            .await
            .map_err(|_| anyhow::anyhow!("unable to verify credential catalog identity"))?;
    let workflow_identity: (String, String) =
        sqlx::query_as("SELECT current_database(), current_schema()")
            .fetch_one(workflow_pool)
            .await
            .map_err(|_| {
                anyhow::anyhow!("unable to verify disposable workflow database identity")
            })?;
    ensure!(
        catalog_identity.0 == "orbit_control_plane"
            && catalog_identity.1 == "public"
            && (catalog_identity.0 != workflow_identity.0
                || catalog_identity.1 != workflow_identity.1)
            && workflow_identity.1 == workflow_schema,
        "live credentials must remain in the separate control-plane catalog"
    );
    Ok(())
}

#[cfg(feature = "fault-injection")]
async fn resolve_live_target(
    catalog_pool: &PgPool,
    role: &RoleDefinition,
    provider: &str,
    reference: Option<&str>,
) -> Result<ResolvedExecutionTarget> {
    let candidates = RoleRuntimeResolver::resolve_ranked_targets_live(
        catalog_pool,
        role,
        None,
        RuntimeQuotaSelectionPolicy::default(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("no eligible account in the authorized credential catalog"))?;
    candidates
        .into_iter()
        .find(|candidate| {
            candidate.provider == provider
                && reference
                    .is_none_or(|reference| candidate.credential_id.as_deref() == Some(reference))
        })
        .context("no eligible account matched the requested provider fixture")
}

#[cfg(feature = "fault-injection")]
async fn guard_live_codex_quota(catalog: &PgPool) -> Result<()> {
    use orbit::codex_status_probe as probe;
    let credential = orbit::credential_registry::CredentialStore::new(catalog)
        .get("codex-main")
        .await?
        .context("Codex qualification credential missing")?;
    let binding = orbit::provider_scope::BindingStore::new(catalog)
        .inspect(&credential.identity())
        .await?
        .context("Codex provider scope missing")?;
    ensure!(
        binding.state == orbit::provider_scope::BindingState::Confirmed,
        "live Codex scope is not confirmed"
    );
    let backend = orbit::secret_backend::LocalPrivateSecretBackend::default_for_operator()?;
    let (runtime, resource) = probe::cataloged_codex_runtime(&credential)?;
    let control = probe::private_control_tempdir()?;
    let outcome = probe::probe_cataloged_once(
        probe::CatalogCredentialSource {
            pool: catalog,
            backend: &backend,
            reference: &credential.reference,
        },
        &runtime,
        &resource,
        probe::ProbeBinding::ConfirmedFingerprint(&binding.fingerprint),
        control.path(),
        Duration::from_secs(60),
    )
    .await
    .map_err(|_| anyhow::anyhow!("live Codex quota probe failed"))?;
    ensure!(
        outcome.receipt.cleanup_confirmed
            && outcome.receipt.authenticated_account_present
            && outcome.receipt.correlated_status_response
            && !outcome.receipt.model_turn_started
            && !outcome.receipt.model_thread_created,
        "live Codex quota receipt incomplete"
    );
    let now = orbit::verification::now_millis();
    ensure!(
        outcome.snapshot.observed_at_ms <= now && now < outcome.snapshot.expires_at_ms,
        "live Codex quota is stale"
    );
    let ordinary_bucket = outcome
        .ordinary_quota_bucket_fingerprint
        .context("live Codex ordinary quota bucket identity is unknown")?;
    let windows = outcome
        .snapshot
        .quota_buckets
        .iter()
        .filter(|bucket| {
            bucket.scope.is_none() && bucket.provider_bucket_fingerprint == ordinary_bucket
        })
        .flat_map(|bucket| bucket.windows.iter())
        .collect::<Vec<_>>();
    let remaining = |duration| {
        windows
            .iter()
            .filter(|window| {
                window.duration_minutes == Some(duration)
                    && window.resets_at_ms.is_some_and(|reset| reset > now)
            })
            .filter_map(|window| {
                window
                    .remaining_percent
                    .or_else(|| window.remaining_fraction.map(|fraction| fraction * 100.0))
                    .or_else(|| window.used_percent.map(|used| 100.0 - used))
            })
            .reduce(f64::min)
    };
    let short = remaining(300).context("live Codex 5h quota is unknown")?;
    let weekly = remaining(10080).context("live Codex weekly quota is unknown")?;
    ensure!(
        short >= 15.0 && weekly >= 5.0,
        "LIVE_CODEX_QUOTA_GUARD: 5h={short:.1}%, weekly={weekly:.1}%; minimum=15%/5%"
    );
    eprintln!(
        "LIVE_CODEX_QUOTA_GUARD: native ordinary meter, confirmed account; 5h={short:.1}%, weekly={weekly:.1}%; minimum=15%/5%"
    );
    Ok(())
}

#[cfg(feature = "fault-injection")]
struct LiveCatalogRoleExecutor {
    catalog: PgPool,
    injected: std::sync::Mutex<std::collections::BTreeSet<String>>,
}

#[cfg(feature = "fault-injection")]
#[async_trait::async_trait]
impl RoleAgentExecutor for LiveCatalogRoleExecutor {
    async fn execute_role(
        &self,
        pool: &PgPool,
        workflow: &WorkflowRun,
        execution: &RoleExecution,
        role: &RoleDefinition,
        target: &ResolvedExecutionTarget,
        task: &str,
        repository: &Path,
        handoff: Option<&HandoffArtifact>,
        cancellation: tokio::sync::watch::Receiver<bool>,
    ) -> Result<RoleExecutionOutcome> {
        let inject = matches!(
            (role.role_id.as_str(), target.provider.as_str()),
            ("planner", "codex") | ("reviewer", "antigravity")
        ) && self.injected.lock().unwrap().insert(role.role_id.clone());
        if inject {
            return RealAcpRoleExecutor
                .simulate_pre_prompt_runtime_failure(pool, execution, target)
                .await;
        }
        if target.provider == "codex" {
            guard_live_codex_quota(&self.catalog).await?;
        }
        RealAcpRoleExecutor
            .execute_role_with_credential_catalog(
                pool,
                &self.catalog,
                workflow,
                execution,
                role,
                target,
                task,
                repository,
                handoff,
                cancellation,
            )
            .await
    }
}

#[cfg(feature = "fault-injection")]
fn sanitized_live_selection_summary(target: &ResolvedExecutionTarget) -> serde_json::Value {
    let fields: BTreeMap<&str, &str> = target
        .resolution_reason
        .split("; ")
        .filter_map(|field| field.split_once('='))
        .collect();
    let rank = target
        .resolution_reason
        .strip_prefix("reset-aware rank=")
        .and_then(|tail| tail.split_once(';'))
        .and_then(|(rank, _)| rank.parse::<u32>().ok());
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
    let quota_percent = |key: &str| {
        fields
            .get(key)
            .and_then(|value| value.strip_suffix('%'))
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
    };
    let reset_at_ms = fields
        .get("7d_reset_at_ms")
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value >= 0);
    let provider_preference_rank = fields
        .get("provider_preference_rank")
        .and_then(|value| value.parse::<u32>().ok());
    let reason_category = match target.resolution_reason.split("; ").nth(1) {
        Some("known_weekly_reset") => "known_weekly_reset",
        Some("weekly_reset_unknown_or_not_applicable") => "weekly_reset_unknown_or_not_applicable",
        _ => "unknown",
    };

    serde_json::json!({
        "provider": target.provider,
        "account_reference": target.credential_id,
        "requested_model": target.requested_model,
        "resolved_model": target.resolved_model,
        "credential_generation": target.credential_generation,
        "availability": availability,
        "five_hour_remaining_percent": quota_percent("5h_remaining"),
        "seven_day_remaining_percent": quota_percent("7d_remaining"),
        "seven_day_reset_at_ms": reset_at_ms,
        "rank": rank,
        "provider_preference_rank": provider_preference_rank,
        "selection_reason_category": reason_category
    })
}

#[cfg(feature = "fault-injection")]
async fn ensure_disposable_schema_has_no_credentials(pool: &PgPool) -> Result<()> {
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM orbit_credentials")
        .fetch_one(pool)
        .await?;
    ensure!(
        count == 0,
        "live provider credentials must not be present in the disposable workflow schema"
    );
    Ok(())
}

async fn teardown_test(ctx: TestContext) -> Result<()> {
    ctx.database.teardown().await
}

#[cfg(feature = "fault-injection")]
async fn finish_live_fixture(
    credential_catalog_pool: PgPool,
    ctx: TestContext,
    fixture_result: Result<()>,
) -> Result<()> {
    credential_catalog_pool.close().await;
    finish_test_context(ctx, fixture_result).await
}

async fn finish_test_context(ctx: TestContext, fixture_result: Result<()>) -> Result<()> {
    let teardown_result = teardown_test(ctx).await;
    match (fixture_result, teardown_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(fixture_error), Ok(())) => Err(fixture_error),
        (Ok(()), Err(teardown_error)) => {
            Err(teardown_error.context("failed to tear down disposable workflow schema"))
        }
        (Err(fixture_error), Err(teardown_error)) => Err(fixture_error.context(format!(
            "disposable workflow schema teardown also failed: {teardown_error:#}"
        ))),
    }
}

async fn load_agent_tool_audit(
    pool: &PgPool,
    execution_id: &str,
) -> Result<(String, i64, i64, i64, serde_json::Value, serde_json::Value)> {
    sqlx::query_as(
        "SELECT status, tool_call_count, tool_success_count, tool_failure_count, tool_counts, metadata \
         FROM orbit_agent_executions WHERE id = $1",
    )
    .bind(execution_id)
    .fetch_one(pool)
    .await
    .context("failed to read persisted agent tool audit")
}

fn b34_audit_has_exact_correlations(audit: &serde_json::Value, expected_count: usize) -> bool {
    let safe_id = |value: &str, limit: usize| {
        !value.is_empty()
            && value.len() <= limit
            && value.is_ascii()
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')
            })
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
    let Some(summary) = audit.get("summary") else {
        return false;
    };
    if audit
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        != Some(2)
        || audit
            .get("correlation_capability")
            .and_then(serde_json::Value::as_str)
            != Some("SUPPORTED")
        || summary
            .get("unmatched_provider_calls")
            .and_then(serde_json::Value::as_u64)
            != Some(0)
        || summary
            .get("unmatched_callbacks")
            .and_then(serde_json::Value::as_u64)
            != Some(0)
        || summary
            .get("callback_count")
            .and_then(serde_json::Value::as_u64)
            != Some(expected_count as u64)
        || summary
            .get("provider_notification_count")
            .and_then(serde_json::Value::as_u64)
            != Some(expected_count as u64)
        || summary.get("total").and_then(serde_json::Value::as_u64) != Some(expected_count as u64)
        || summary
            .get("mutating")
            .and_then(serde_json::Value::as_u64)
            .is_none()
        || summary
            .get("successful")
            .and_then(serde_json::Value::as_u64)
            != Some(expected_count as u64)
        || summary
            .get("unsuccessful")
            .and_then(serde_json::Value::as_u64)
            != Some(0)
        || summary.get("denied").and_then(serde_json::Value::as_u64) != Some(0)
        || summary
            .get("mutating_unknown")
            .and_then(serde_json::Value::as_u64)
            != Some(0)
        || audit
            .get("omitted_count")
            .and_then(serde_json::Value::as_u64)
            != Some(0)
        || audit
            .get("provider_tool_names_omitted")
            .and_then(serde_json::Value::as_u64)
            != Some(0)
        || entries.len() != expected_count
        || provider_updates.len() != expected_count
    {
        return false;
    }

    let mut updates_by_invocation = BTreeMap::new();
    let mut seen_update_provider_ids = std::collections::BTreeSet::new();
    for update in provider_updates {
        let Some(invocation_id) = update
            .get("tool_invocation_id")
            .and_then(serde_json::Value::as_str)
        else {
            return false;
        };
        let Some(provider_id) = update
            .get("provider_tool_call_id")
            .and_then(serde_json::Value::as_str)
        else {
            return false;
        };
        if update
            .get("correlation_state")
            .and_then(serde_json::Value::as_str)
            != Some("OBSERVED")
            || !safe_id(invocation_id, 128)
            || !safe_id(provider_id, 256)
            || updates_by_invocation
                .insert(invocation_id, provider_id)
                .is_some()
            || !seen_update_provider_ids.insert(provider_id)
        {
            return false;
        }
    }

    let mut seen_invocations = std::collections::BTreeSet::new();
    let mut seen_provider_ids = std::collections::BTreeSet::new();
    let mut seen_callbacks = std::collections::BTreeSet::new();
    let mut seen_sequences = std::collections::BTreeSet::new();
    let mut mutating_count = 0u64;
    for entry in entries {
        let sequence = entry.get("sequence").and_then(serde_json::Value::as_u64);
        let mutating = entry.get("mutating").and_then(serde_json::Value::as_bool);
        if sequence.is_none_or(|value| {
            value == 0 || value > expected_count as u64 || !seen_sequences.insert(value)
        }) || mutating.is_none()
            || entry
                .get("turn_completed")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
        {
            return false;
        }
        if mutating == Some(true) {
            mutating_count = mutating_count.saturating_add(1);
        }
        let invocation_id = entry
            .get("tool_invocation_id")
            .and_then(serde_json::Value::as_str);
        let provider_id = entry
            .get("provider_tool_call_id")
            .and_then(serde_json::Value::as_str);
        let callback_id = entry
            .get("callback_request_id")
            .and_then(serde_json::Value::as_str);
        let provider_name = entry
            .get("provider_tool_name")
            .and_then(serde_json::Value::as_str);
        let canonical_name = entry
            .get("canonical_tool_name")
            .and_then(serde_json::Value::as_str);
        let provider_method_matches = provider_name
            .and_then(orbit::tool_surface::CanonicalToolName::from_wire)
            .is_some_and(|tool| canonical_name == Some(tool.as_str()));
        if entry.get("outcome").and_then(serde_json::Value::as_str) != Some("SUCCESS")
            || entry
                .get("terminal_state")
                .and_then(serde_json::Value::as_str)
                != Some("SUCCESS")
            || entry
                .get("provider_update_correlation")
                .and_then(serde_json::Value::as_str)
                != Some("CORRELATED")
            || entry
                .get("provider_name_mapping")
                .and_then(serde_json::Value::as_str)
                != Some("MATCH")
            || !provider_method_matches
            || entry
                .get("advertised_to_provider")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
            || entry
                .get("role_allowed")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
            || !entry
                .get("error_code")
                .is_none_or(serde_json::Value::is_null)
            || entry
                .get("callback_request_id_shape")
                .and_then(serde_json::Value::as_str)
                != Some("valid")
            || entry
                .get("provider_tool_call_id_shape")
                .and_then(serde_json::Value::as_str)
                != Some("string")
            || entry
                .get("provider_update_title_class")
                .and_then(serde_json::Value::as_str)
                != Some("non_empty_string")
            || !entry
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
            || entry
                .get("provider_update_status")
                .and_then(serde_json::Value::as_str)
                != Some("in_progress")
            || invocation_id.is_none_or(|id| !safe_id(id, 128))
            || provider_id.is_none_or(|id| !safe_id(id, 256))
            || callback_id.is_none_or(|id| !safe_id(id, 130))
            || invocation_id.is_none_or(|id| !seen_invocations.insert(id))
            || provider_id.is_none_or(|id| !seen_provider_ids.insert(id))
            || callback_id.is_none_or(|id| !seen_callbacks.insert(id))
            || invocation_id.and_then(|id| updates_by_invocation.remove(id)) != provider_id
        {
            return false;
        }
    }
    updates_by_invocation.is_empty()
        && seen_sequences.len() == expected_count
        && summary.get("mutating").and_then(serde_json::Value::as_u64) == Some(mutating_count)
}

#[test]
fn b34_audit_correlation_requires_unique_terminal_rows() {
    let valid = json!({
        "schema_version": 2,
        "correlation_capability": "SUPPORTED",
        "summary": {
            "total": 2,
            "mutating": 0,
            "callback_count": 2,
            "provider_notification_count": 2,
            "successful": 2,
            "unsuccessful": 0,
            "denied": 0,
            "mutating_unknown": 0,
            "unmatched_provider_calls": 0,
            "unmatched_callbacks": 0
        },
        "omitted_count": 0,
        "provider_tool_names_omitted": 0,
        "entries": [
            {
                "sequence": 1,
                "tool_invocation_id": "oti-a",
                "provider_tool_call_id": "provider-a",
                "callback_request_id": "s:rpc-a",
                "callback_request_id_shape": "valid",
                "provider_tool_call_id_shape": "string",
                "provider_tool_name": "fs.read_text_file",
                "provider_name_mapping": "MATCH",
                "canonical_tool_name": "fs.read_text_file",
                "advertised_to_provider": true,
                "role_allowed": true,
                "provider_update_correlation": "CORRELATED",
                "provider_update_title_class": "non_empty_string",
                "provider_update_tool_kind": "read",
                "provider_update_status": "in_progress",
                "terminal_state": "SUCCESS",
                "outcome": "SUCCESS",
                "mutating": false,
                "turn_completed": true,
                "error_code": null
            },
            {
                "sequence": 2,
                "tool_invocation_id": "oti-b",
                "provider_tool_call_id": "provider-b",
                "callback_request_id": "s:rpc-b",
                "callback_request_id_shape": "valid",
                "provider_tool_call_id_shape": "string",
                "provider_tool_name": "fs/read_text_file",
                "provider_name_mapping": "MATCH",
                "canonical_tool_name": "fs.read_text_file",
                "advertised_to_provider": true,
                "role_allowed": true,
                "provider_update_correlation": "CORRELATED",
                "provider_update_title_class": "non_empty_string",
                "provider_update_tool_kind": "read",
                "provider_update_status": "in_progress",
                "terminal_state": "SUCCESS",
                "outcome": "SUCCESS",
                "mutating": false,
                "turn_completed": true,
                "error_code": null
            }
        ],
        "provider_updates": [
            {
                "tool_invocation_id": "oti-a",
                "provider_tool_call_id": "provider-a",
                "correlation_state": "OBSERVED"
            },
            {
                "tool_invocation_id": "oti-b",
                "provider_tool_call_id": "provider-b",
                "correlation_state": "OBSERVED"
            }
        ]
    });
    assert!(b34_audit_has_exact_correlations(&valid, 2));

    let mut duplicate_invocation = valid.clone();
    duplicate_invocation["entries"][1]["tool_invocation_id"] =
        duplicate_invocation["entries"][0]["tool_invocation_id"].clone();
    assert!(!b34_audit_has_exact_correlations(&duplicate_invocation, 2));

    let mut duplicate_callback = valid.clone();
    duplicate_callback["entries"][1]["callback_request_id"] =
        duplicate_callback["entries"][0]["callback_request_id"].clone();
    assert!(!b34_audit_has_exact_correlations(&duplicate_callback, 2));

    let mut mismatched_update = valid.clone();
    mismatched_update["provider_updates"][1]["provider_tool_call_id"] =
        json!("different-provider-id");
    assert!(!b34_audit_has_exact_correlations(&mismatched_update, 2));

    let mut extra_update = valid.clone();
    extra_update["provider_updates"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "tool_invocation_id": "oti-extra",
            "provider_tool_call_id": "provider-extra",
            "correlation_state": "OBSERVED"
        }));
    assert!(!b34_audit_has_exact_correlations(&extra_update, 2));

    let mut ambiguous_row = valid.clone();
    ambiguous_row["entries"][0]["provider_update_correlation"] = json!("UNRESOLVED");
    assert!(!b34_audit_has_exact_correlations(&ambiguous_row, 2));

    let mut legacy_schema = valid.clone();
    legacy_schema["schema_version"] = json!(1);
    assert!(!b34_audit_has_exact_correlations(&legacy_schema, 2));

    for (field, value) in [
        ("advertised_to_provider", json!(false)),
        ("role_allowed", json!(false)),
        ("error_code", json!("TOOL_EXECUTION_FAILED")),
        ("canonical_tool_name", json!("fs.write_text_file")),
        ("provider_update_title_class", json!("missing")),
        ("provider_update_tool_kind", json!("unrecognized")),
        ("provider_update_status", json!("completed")),
    ] {
        let mut invalid_row = valid.clone();
        invalid_row["entries"][0][field] = value;
        assert!(!b34_audit_has_exact_correlations(&invalid_row, 2));
    }

    for field in ["denied", "mutating_unknown"] {
        let mut invalid_summary = valid.clone();
        invalid_summary["summary"][field] = json!(1);
        assert!(!b34_audit_has_exact_correlations(&invalid_summary, 2));
    }

    let mut missing_mutating = valid.clone();
    missing_mutating["entries"][0]
        .as_object_mut()
        .unwrap()
        .remove("mutating");
    assert!(!b34_audit_has_exact_correlations(&missing_mutating, 2));

    let mut inconsistent_mutating = valid.clone();
    inconsistent_mutating["entries"][0]["mutating"] = json!(true);
    assert!(!b34_audit_has_exact_correlations(&inconsistent_mutating, 2));
    inconsistent_mutating["summary"]["mutating"] = json!(1);
    assert!(b34_audit_has_exact_correlations(&inconsistent_mutating, 2));

    let mut incomplete_turn = valid.clone();
    incomplete_turn["entries"][0]["turn_completed"] = json!(false);
    assert!(!b34_audit_has_exact_correlations(&incomplete_turn, 2));

    let mut duplicate_sequence = valid.clone();
    duplicate_sequence["entries"][1]["sequence"] = json!(1);
    assert!(!b34_audit_has_exact_correlations(&duplicate_sequence, 2));
}

#[cfg(feature = "fault-injection")]
async fn load_role_tool_audits(
    pool: &PgPool,
    role_execution_id: &str,
) -> Result<
    Vec<(
        String,
        String,
        i64,
        i64,
        i64,
        Option<String>,
        serde_json::Value,
    )>,
> {
    sqlx::query_as(
        "SELECT id, status, tool_call_count, tool_success_count, tool_failure_count, actual_model, metadata \
         FROM orbit_agent_executions \
         WHERE role_execution_id = $1 ORDER BY started_at_ms, id",
    )
    .bind(role_execution_id)
    .fetch_all(pool)
    .await
    .context("failed to read role tool-call audit")
}

#[expect(
    clippy::too_many_arguments,
    reason = "the bounded fixture report names each durable execution fact"
)]
fn render_live_fixture_audit(
    role_execution_id: &str,
    agent_execution_id: &str,
    status: &str,
    tool_call_count: i64,
    tool_success_count: i64,
    tool_failure_count: i64,
    actual_model: Option<&str>,
    metadata: &serde_json::Value,
) -> String {
    let safe_id = |value: &str| {
        if value.len() <= 96
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            value.to_owned()
        } else {
            "redacted".to_owned()
        }
    };
    let safe_status = match status {
        "SUCCEEDED" | "FAILED" | "CANCELLED" | "TIMEOUT" | "RUNNING" => status,
        _ => "UNKNOWN",
    };
    let actual_model = match actual_model {
        Some(value)
            if !value.is_empty()
                && value.len() <= 128
                && value.is_ascii()
                && value.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')
                }) =>
        {
            value.to_owned()
        }
        Some(_) => "observed_but_redacted".to_owned(),
        None => "UNKNOWN / unobserved".to_owned(),
    };
    let summary = metadata
        .get("tool_call_audit")
        .and_then(|audit| audit.get("summary"));
    let count = |field: &str| {
        summary
            .and_then(|summary| summary.get(field))
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0)
    };
    let lifecycle = metadata.get("lifecycle");
    let lifecycle_label = |field: &str, allowed: &[&str]| {
        lifecycle
            .and_then(|lifecycle| lifecycle.get(field))
            .and_then(serde_json::Value::as_str)
            .filter(|value| allowed.contains(value))
            .unwrap_or("UNKNOWN")
    };
    let milestones_contain_tool_activity = lifecycle
        .and_then(|lifecycle| lifecycle.get("milestones"))
        .and_then(serde_json::Value::as_array)
        .is_some_and(|milestones| {
            milestones
                .iter()
                .any(|value| value == "TOOL_ACTIVITY_OBSERVED")
        });
    let audit_present = metadata
        .get("tool_call_audit")
        .is_some_and(serde_json::Value::is_object);
    let audit_applicability = if lifecycle_label(
        "tool_audit_applicability",
        &["NOT_APPLICABLE_BEFORE_TOOL_PHASE", "APPLICABLE"],
    ) == "NOT_APPLICABLE_BEFORE_TOOL_PHASE"
        && !milestones_contain_tool_activity
    {
        "NOT_APPLICABLE_BEFORE_TOOL_PHASE"
    } else if audit_present {
        "AVAILABLE"
    } else {
        "UNAVAILABLE_EVIDENCE_DEFECT"
    };
    let process_code = lifecycle
        .and_then(|lifecycle| lifecycle.get("process_exit_code"))
        .and_then(serde_json::Value::as_i64)
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let process_signal = lifecycle
        .and_then(|lifecycle| lifecycle.get("process_signal"))
        .and_then(serde_json::Value::as_i64)
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let reason = lifecycle
        .and_then(|lifecycle| lifecycle.get("normalized_reason"))
        .and_then(serde_json::Value::as_str)
        .filter(|value| {
            value.len() <= 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        })
        .unwrap_or("UNKNOWN");
    let supervisor_failure =
        match lifecycle.and_then(|lifecycle| lifecycle.get("supervisor_failure")) {
            Some(serde_json::Value::Null) => "NONE",
            Some(serde_json::Value::String(value))
                if [
                    "SUPERVISOR_SIGNALED",
                    "SUPERVISOR_NONZERO_EXIT",
                    "SUPERVISOR_EXIT_UNCONFIRMED",
                    "CLEANUP_RECEIPT_MISMATCH",
                    "CLEANUP_RECEIPT_UNCONFIRMED",
                ]
                .contains(&value.as_str()) =>
            {
                value
            }
            _ => "UNKNOWN",
        };
    let failed_phase = match lifecycle.and_then(|lifecycle| lifecycle.get("failed_phase")) {
        Some(serde_json::Value::Null) => "NONE",
        Some(serde_json::Value::String(value))
            if [
                "AGENT_EXECUTION_CREATED",
                "CREDENTIAL_RESOLUTION",
                "CREDENTIAL_RESOLVED",
                "CREDENTIAL_STAGING",
                "CREDENTIAL_STAGED",
                "RUNTIME_PREPARATION",
                "RUNTIME_PREPARED",
                "SUPERVISOR_START",
                "SUPERVISOR_STARTED",
                "ACP_INITIALIZE",
                "ACP_INITIALIZED",
                "SESSION_CREATION",
                "SESSION_CREATED",
                "PROMPT_DISPATCH",
                "PROMPT_IN_FLIGHT",
                "PROMPT_RESPONSE_RECEIVED",
                "TOOL_ACTIVITY",
                "CLEANUP",
                "SUPERVISOR_EXIT_OBSERVED",
                "CLEANUP_CONFIRMED",
                "CLEANUP_UNCONFIRMED",
            ]
            .contains(&value.as_str()) =>
        {
            value
        }
        _ => "UNKNOWN",
    };
    let receipt = lifecycle.and_then(|lifecycle| lifecycle.get("supervisor_receipt"));
    let receipt_summary = receipt.map(|receipt| {
        let version = receipt
            .get("format_version")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let runtime = match receipt.get("runtime").and_then(serde_json::Value::as_str) {
            Some("podman") => "podman",
            _ => "unknown",
        };
        let launch_stage = match receipt.get("launch_stage").and_then(serde_json::Value::as_str) {
            Some("app_server_protocol") => "app_server_protocol",
            Some("completed_turn_cleanup") => "completed_turn_cleanup",
            Some("supervisor_deadline") => "supervisor_deadline",
            Some("container_startup") => "container_startup",
            Some("transport") => "transport",
            Some("container_process") => "container_process",
            Some("unknown") => "unknown",
            _ => "unknown",
        };
        let image_matches = receipt
            .get("expected_image_matches")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let container_exit_code = receipt
            .get("exit_code")
            .and_then(serde_json::Value::as_i64)
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let diagnostic_present = receipt
            .get("diagnostic_present")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let diagnostic_truncated = receipt
            .get("diagnostic_truncated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let session_summary = receipt
            .get("codex_session")
            .filter(|value| value.is_object())
            .map(|session| {
                let label = |field: &str, allowed: &[&str]| {
                    session
                        .get(field)
                        .and_then(serde_json::Value::as_str)
                        .filter(|value| allowed.contains(value))
                        .unwrap_or("unknown")
                };
                let request_count = session
                    .get("server_request_count")
                    .and_then(serde_json::Value::as_u64)
                    .map(|value| value.min(64))
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                let bool_field = |field: &str| {
                    session
                        .get(field)
                        .and_then(serde_json::Value::as_bool)
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "unknown".to_string())
                };
                format!(
                    "Codex session receipt: trigger={}, phase={}, last_activity={}, pending_request={}, outcome={}, turn_outcome={}, server_request_count={request_count}, peer_eof_observed={}, app_server_stdout_eof_observed={}",
                    label("supervisor_trigger", &["bridge_returned", "bridge_error", "peer_eof_after_end_turn", "child_exit", "supervisor_deadline"]),
                    label("phase", &["bridge_start", "initializing", "session_setup", "turn_start", "cancelling", "protocol", "bridge_request", "session_ready", "prompt_received", "turn_running", "turn_completed"]),
                    label("last_activity", &["none", "app_server_request_sent", "server_request_received", "dynamic_tool_request_received", "server_error_notification", "turn_started_notification", "turn_completed_notification", "item_lifecycle_notification", "agent_message_notification", "server_warning_notification", "server_notification", "app_server_stdout_eof", "app_server_stdout_read_error", "acp_peer_eof", "acp_peer_read_error", "response_correlation_failure", "correlated_protocol_rejection", "app_server_response_received", "server_request_rejected", "app_server_error_notification", "acp_request_received", "initialized_notification_sent", "session_created", "prompt_received", "acp_request_rejected", "acp_end_turn_response_sent", "acp_cancel_received", "turn_start_response_received", "dynamic_tool_result_sent", "turn_completed_not_successful"]),
                    label("pending_request", &["none", "initialize", "account_read", "thread_start", "turn_start", "turn_interrupt", "other_request"]),
                    label("outcome", &["running", "app_server_eof", "protocol_read_failure", "peer_eof_after_end_turn", "peer_eof", "correlation_failure", "protocol_rejection", "server_request_rejected", "app_server_error_notification", "cancelled", "turn_incomplete", "end_turn", "tool_callback_failed"]),
                    label("turn_outcome", &["not_started", "start_pending", "running", "end_turn", "app_server_error_notification"]),
                    bool_field("peer_eof_observed"),
                    bool_field("app_server_stdout_eof_observed")
                )
            });
        let mut summary = format!(
            "Supervisor receipt: version={version}, runtime={runtime}, launch_stage={launch_stage}, container_exit_code={container_exit_code}, expected_image_matches={image_matches}, diagnostic_present={diagnostic_present}, diagnostic_truncated={diagnostic_truncated}"
        );
        if let Some(session) = session_summary {
            summary.push('\n');
            summary.push_str(&session);
        }
        summary
    });
    format!(
        "RoleExecution ID: {}\nAgentExecution ID: {}\nExecution status: {safe_status}\nActual model: {actual_model}\nACP lifecycle: phase={}, attempted_phase={}, failed_phase={failed_phase}, last_confirmed_phase={}, outcome={}, normalized_reason={reason}, prompt_uncertainty={}, process_exit_code={process_code}, process_signal={process_signal}, supervisor_outcome={}, supervisor_failure={supervisor_failure}, cleanup_state={}, persistence_state={}\nTool audit applicability: {audit_applicability}\nAggregate tool counts: total={tool_call_count}, success={tool_success_count}, failure={tool_failure_count}, mutating={}, denied={}, unmatched={}\n{}\n{}",
        safe_id(role_execution_id),
        safe_id(agent_execution_id),
        lifecycle_label(
            "phase",
            &[
                "AGENT_EXECUTION_CREATED",
                "CREDENTIAL_RESOLUTION",
                "CREDENTIAL_RESOLVED",
                "CREDENTIAL_STAGING",
                "CREDENTIAL_STAGED",
                "RUNTIME_PREPARATION",
                "RUNTIME_PREPARED",
                "SUPERVISOR_START",
                "SUPERVISOR_STARTED",
                "ACP_INITIALIZE",
                "ACP_INITIALIZED",
                "SESSION_CREATION",
                "SESSION_CREATED",
                "PROMPT_DISPATCH",
                "PROMPT_IN_FLIGHT",
                "PROMPT_RESPONSE_RECEIVED",
                "TOOL_ACTIVITY",
                "CLEANUP",
                "SUPERVISOR_EXIT_OBSERVED",
                "CLEANUP_CONFIRMED",
                "CLEANUP_UNCONFIRMED",
                "TERMINAL"
            ]
        ),
        lifecycle_label(
            "attempted_phase",
            &[
                "AGENT_EXECUTION_CREATED",
                "CREDENTIAL_RESOLUTION",
                "CREDENTIAL_RESOLVED",
                "CREDENTIAL_STAGING",
                "CREDENTIAL_STAGED",
                "RUNTIME_PREPARATION",
                "RUNTIME_PREPARED",
                "SUPERVISOR_START",
                "SUPERVISOR_STARTED",
                "ACP_INITIALIZE",
                "ACP_INITIALIZED",
                "SESSION_CREATION",
                "SESSION_CREATED",
                "PROMPT_DISPATCH",
                "PROMPT_IN_FLIGHT",
                "PROMPT_RESPONSE_RECEIVED",
                "TOOL_ACTIVITY",
                "CLEANUP",
                "SUPERVISOR_EXIT_OBSERVED",
                "CLEANUP_CONFIRMED",
                "CLEANUP_UNCONFIRMED",
                "TERMINAL"
            ]
        ),
        lifecycle_label(
            "last_confirmed_phase",
            &[
                "AGENT_EXECUTION_CREATED",
                "CREDENTIAL_RESOLVED",
                "CREDENTIAL_STAGED",
                "RUNTIME_PREPARED",
                "SUPERVISOR_STARTED",
                "ACP_INITIALIZED",
                "SESSION_CREATED",
                "PROMPT_RESPONSE_RECEIVED",
                "SUPERVISOR_EXIT_OBSERVED",
                "CLEANUP_CONFIRMED"
            ]
        ),
        lifecycle_label("outcome", &["IN_PROGRESS", "SUCCEEDED", "FAILED"]),
        lifecycle_label(
            "prompt_uncertainty",
            &[
                "NOT_DISPATCHED",
                "IN_FLIGHT",
                "RESOLVED",
                "UNRESOLVED_PENDING_MODEL_CALL",
                "UNRESOLVED_NO_PENDING_MODEL_CALL",
                "UNRESOLVED_UNKNOWN"
            ]
        ),
        lifecycle_label(
            "supervisor_outcome",
            &[
                "NOT_OBSERVED",
                "EXITED_ZERO",
                "EXITED_NONZERO",
                "SIGNALED",
                "EXIT_UNCONFIRMED",
            ]
        ),
        lifecycle_label(
            "cleanup_state",
            &["NO_RUNTIME_RESOURCE_CREATED", "UNCONFIRMED", "CONFIRMED"]
        ),
        lifecycle_label("persistence_state", &["CONFIRMED", "UNCONFIRMED"]),
        count("mutating"),
        count("denied"),
        count("unmatched_provider_calls"),
        receipt_summary.unwrap_or_default(),
        render_tool_call_audit(metadata),
    )
}

#[derive(Clone, Copy)]
enum AgentExecutionLookupFailure {
    #[cfg(feature = "fault-injection")]
    MissingRoleExecutionId,
    QueryFailed,
    NoRows,
}

fn render_agent_execution_lookup_failure(failure: AgentExecutionLookupFailure) -> &'static str {
    match failure {
        #[cfg(feature = "fault-injection")]
        AgentExecutionLookupFailure::MissingRoleExecutionId => {
            "AgentExecution query not attempted: role execution ID missing; tool audit: UNAVAILABLE_EVIDENCE_DEFECT."
        }
        AgentExecutionLookupFailure::QueryFailed => {
            "AgentExecution query failed; lifecycle evidence unavailable; tool audit: UNAVAILABLE_EVIDENCE_DEFECT."
        }
        AgentExecutionLookupFailure::NoRows => {
            "AgentExecution row missing; lifecycle evidence unavailable; tool audit: UNAVAILABLE_EVIDENCE_DEFECT."
        }
    }
}

#[cfg(feature = "fault-injection")]
async fn print_live_fixture_audit(pool: &PgPool, role_execution_id: Option<&str>) {
    println!("Live fixture execution audit:");
    let Some(role_execution_id) = role_execution_id else {
        println!(
            "{}",
            render_agent_execution_lookup_failure(
                AgentExecutionLookupFailure::MissingRoleExecutionId
            )
        );
        return;
    };
    match load_role_tool_audits(pool, role_execution_id).await {
        Ok(execution_rows) if !execution_rows.is_empty() => {
            for (execution_id, status, calls, successes, failures, actual_model, metadata) in
                execution_rows
            {
                println!(
                    "{}",
                    render_live_fixture_audit(
                        role_execution_id,
                        &execution_id,
                        &status,
                        calls,
                        successes,
                        failures,
                        actual_model.as_deref(),
                        &metadata,
                    )
                );
            }
        }
        Ok(_) => println!(
            "{}",
            render_agent_execution_lookup_failure(AgentExecutionLookupFailure::NoRows)
        ),
        Err(_) => println!(
            "{}",
            render_agent_execution_lookup_failure(AgentExecutionLookupFailure::QueryFailed)
        ),
    }
}

#[test]
fn live_fixture_audit_report_includes_ids_counts_and_bounded_safe_details() {
    let entries = (1..=100)
        .map(|sequence| {
            json!({
                "sequence": sequence,
                "provider_tool_name": "orbit_read_file",
                "provider_name_mapping": "MATCH",
                "canonical_tool_name": "fs.read_text_file",
                "advertised_to_provider": true,
                "role_allowed": true,
                "outcome": "SUCCESS",
                "error_code": null,
                "mutating": false,
                "mutation_applied": false,
                "later_callback_observed": false,
                "turn_completed": true,
                "path_arguments": [{
                    "argument": "path",
                    "state": "WORKSPACE_RELATIVE",
                    "workspace_relative_path": "/private/host/path",
                    "exists": true,
                    "display_truncated": false
                }]
            })
        })
        .collect::<Vec<_>>();
    let metadata = json!({
        "tool_call_audit": {
            "summary": {
                "total": 100,
                "successful": 100,
                "unsuccessful": 0,
                "mutating": 2,
                "mutating_unknown": 0,
                "denied": 0,
                "unmatched_provider_calls": 0
            },
            "entries": entries,
            "omitted_count": 36,
            "provider_tool_names_omitted": 0
        }
    });

    let report = render_live_fixture_audit(
        "role-execution-123",
        "agent-execution-456",
        "SUCCEEDED",
        100,
        100,
        0,
        None,
        &metadata,
    );

    assert!(report.contains("RoleExecution ID: role-execution-123"));
    assert!(report.contains("AgentExecution ID: agent-execution-456"));
    assert!(report.contains("Execution status: SUCCEEDED"));
    assert!(report.contains("Actual model: UNKNOWN / unobserved"));
    assert!(report.contains("Tool audit applicability: AVAILABLE"));
    assert!(report.contains(
        "Aggregate tool counts: total=100, success=100, failure=0, mutating=2, denied=0, unmatched=0"
    ));
    assert!(report.contains("provider titles omitted=0"));
    assert!(report.contains("path=INVALID_PATH"));
    assert!(!report.contains("/private/host/path"));
    assert!(report.matches("orbit_read_file | MATCH").count() <= 64);
    assert!(report.len() < 20_000);
}

#[test]
fn lifecycle_report_distinguishes_pre_tool_phase_from_missing_audit_evidence() {
    let pre_tool = json!({
        "tool_call_audit": { "summary": { "total": 0 }, "entries": [] },
        "lifecycle": {
            "phase": "ACP_INITIALIZE",
            "attempted_phase": "ACP_INITIALIZE",
            "failed_phase": "ACP_INITIALIZE",
            "last_confirmed_phase": "SUPERVISOR_STARTED",
            "outcome": "FAILED",
            "normalized_reason": "ACP_INITIALIZE_FAILED",
            "tool_audit_applicability": "NOT_APPLICABLE_BEFORE_TOOL_PHASE",
            "milestones": ["TARGET_COMMITTED", "AGENT_EXECUTION_CREATED", "SUPERVISOR_STARTED"],
            "cleanup_state": "CONFIRMED",
            "persistence_state": "CONFIRMED",
            "prompt_uncertainty": "NOT_DISPATCHED",
            "process_exit_code": 125,
            "supervisor_outcome": "EXITED_NONZERO",
            "supervisor_failure": "SUPERVISOR_NONZERO_EXIT",
            "supervisor_receipt": {
                "format_version": 4,
                "runtime": "podman",
                "launch_stage": "container_startup",
                "expected_image_matches": true,
                "diagnostic_present": true,
                "diagnostic_truncated": true,
                "raw_output": "private payload must not appear",
                "codex_session": {
                    "supervisor_trigger": "bridge_error",
                    "phase": "session_setup",
                    "last_activity": "app_server_response_received",
                    "pending_request": "thread_start",
                    "outcome": "app_server_error_notification",
                    "turn_outcome": "start_pending",
                    "server_request_count": 2,
                    "peer_eof_observed": false,
                    "app_server_stdout_eof_observed": false
                }
            }
        }
    });
    let report = render_live_fixture_audit("role-1", "agent-1", "FAILED", 0, 0, 0, None, &pre_tool);
    assert!(report.contains("Tool audit applicability: NOT_APPLICABLE_BEFORE_TOOL_PHASE"));
    assert!(report.contains("normalized_reason=ACP_INITIALIZE_FAILED"));
    assert!(report.contains("attempted_phase=ACP_INITIALIZE, failed_phase=ACP_INITIALIZE"));
    assert!(report.contains("last_confirmed_phase=SUPERVISOR_STARTED"));
    assert!(
        report.contains(
            "supervisor_outcome=EXITED_NONZERO, supervisor_failure=SUPERVISOR_NONZERO_EXIT"
        )
    );
    assert!(
        report.contains(
            "Supervisor receipt: version=4, runtime=podman, launch_stage=container_startup"
        )
    );
    assert!(report.contains("Codex session receipt: trigger=bridge_error, phase=session_setup"));
    assert!(report.contains("pending_request=thread_start, outcome=app_server_error_notification"));
    assert!(!report.contains("private payload must not appear"));

    let missing_after_tool = json!({
        "lifecycle": {
            "phase": "TOOL_ACTIVITY",
            "last_confirmed_phase": "SESSION_CREATED",
            "outcome": "FAILED",
            "tool_audit_applicability": "APPLICABLE",
            "milestones": ["SESSION_CREATED", "TOOL_ACTIVITY_OBSERVED"]
        }
    });
    let report = render_live_fixture_audit(
        "role-1",
        "agent-1",
        "FAILED",
        1,
        0,
        1,
        None,
        &missing_after_tool,
    );
    assert!(report.contains("Tool audit applicability: UNAVAILABLE_EVIDENCE_DEFECT"));
    assert!(report.contains("Tool-call audit unavailable."));

    assert_ne!(
        render_agent_execution_lookup_failure(AgentExecutionLookupFailure::NoRows),
        render_agent_execution_lookup_failure(AgentExecutionLookupFailure::QueryFailed)
    );
    assert!(
        render_agent_execution_lookup_failure(AgentExecutionLookupFailure::NoRows)
            .contains("AgentExecution row missing")
    );
    assert!(
        render_agent_execution_lookup_failure(AgentExecutionLookupFailure::QueryFailed)
            .contains("AgentExecution query failed")
    );
}

#[test]
fn rendered_tool_audit_contains_only_bounded_safe_correlation_ids() {
    let metadata = json!({
        "tool_call_audit": {
            "summary": {
                "total": 1,
                "successful": 1,
                "unsuccessful": 0,
                "mutating": 1,
                "denied": 0,
                "unmatched_provider_calls": 0,
                "unmatched_callbacks": 0
            },
            "correlation_capability": "SUPPORTED",
            "provider_updates": [{
                "tool_invocation_id": "oti-safe-1",
                "provider_tool_call_id": "provider-safe-1",
                "correlation_state": "OBSERVED"
            }],
            "entries": [{
                "sequence": 1,
                "tool_invocation_id": "oti-safe-1",
                "provider_tool_call_id": "provider-safe-1",
                "callback_request_id": "s:rpc-safe-1",
                "provider_tool_name": "fs.write_text_file",
                "provider_name_mapping": "MATCH",
                "provider_update_correlation": "CORRELATED",
                "provider_update_title_class": "non_empty_string",
                "provider_update_tool_kind": "edit",
                "provider_update_status": "in_progress",
                "provider_tool_call_id_shape": "string",
                "callback_request_id_shape": "valid",
                "canonical_tool_name": "fs.write_text_file",
                "advertised_to_provider": true,
                "role_allowed": true,
                "outcome": "SUCCESS",
                "terminal_state": "SUCCESS",
                "mutating": true,
                "mutation_applied": true,
                "error_code": null,
                "path_arguments": []
            }],
            "omitted_count": 0,
            "provider_tool_names_omitted": 0
        }
    });
    let report = render_tool_call_audit(&metadata);
    assert!(report.contains("oti-safe-1"));
    assert!(report.contains("provider-safe-1"));
    assert!(report.contains("s:rpc-safe-1"));
    assert!(report.contains(
        "provider notifications=1 (recorded=1, omitted=0), callbacks=1, correlated terminal rows=1"
    ));

    let mut unsafe_id = metadata;
    unsafe_id["tool_call_audit"]["entries"][0]["provider_tool_call_id"] =
        json!("/private/provider/payload");
    unsafe_id["tool_call_audit"]["entries"][0]["provider_update_correlation"] = json!("UNMATCHED");
    unsafe_id["tool_call_audit"]["entries"][0]["terminal_state"] = json!("UNRESOLVED");
    let redacted_report = render_tool_call_audit(&unsafe_id);
    assert!(redacted_report.contains("redacted"));
    assert!(redacted_report.contains("UNRESOLVED"));
    assert!(!redacted_report.contains("/private/provider/payload"));
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn agent_tool_audit_reads_persisted_counter_columns() -> Result<()> {
    let ctx = setup_test().await?;
    let fixture_result = async {
        let repo = tempdir()?;
        fs::write(repo.path().join("README.md"), "fixture read\n")?;
        let attempt_id = format!("att-{}", id());
        let repo_path = repo.path().to_string_lossy().into_owned();
        let workflow = ctx
            .store
            .create_workflow_run_full(
                "software_change_v1",
                &attempt_id,
                1,
                None,
                None,
                None,
                Some("read persisted callback audit counters"),
                Some(&repo_path),
                None,
            )
            .await?;
        let role = RoleDefinition::implementer_v1();
        let role_execution = ctx
            .store
            .create_role_execution(&workflow.id, &role, "IMPLEMENTING", 0, None, None)
            .await?;
        let (agent_execution_id, target) =
            start_fixture_agent_execution(&ctx, &role_execution).await?;

        let mut state = AcpTurnState::new(repo.path(), WorkspaceAccess::ReadWrite);
        state.role_id = Some("implementer".into());
        state.workspace_identity = Some(repo.path().canonicalize()?.to_string_lossy().into_owned());
        state.wf_attempt_id = Some(attempt_id);
        state.role_exec_id = Some(role_execution.id.clone());
        state.agent_exec_id = Some(agent_execution_id.clone());
        state.pool = Some(&ctx.engine.pool);

        let (server_in, client_out) = tokio::io::duplex(65536);
        let (client_in, server_out) = tokio::io::duplex(65536);
        let mut server_wire = Wire::new(server_in, server_out, 16 * 1024 * 1024);
        let mut client_wire = Wire::new(client_in, client_out, 16 * 1024 * 1024);
        let response = dispatch_correlated_fixture_callback(
            &mut server_wire,
            &mut client_wire,
            &mut state,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "fs/read_text_file",
                "params": {"path": "README.md"}
            }),
        )
        .await?;
        ensure!(
            response["result"]["content"].as_str() == Some("fixture read\n"),
            "correlated fixture read returned unexpected content"
        );
        ensure!(state.tool_calls == 1 && state.tool_successes == 1 && state.tool_failures == 0,
            "correlated fixture read produced unexpected counters");
        finish_fixture_agent_execution(
            &ctx,
            &role_execution,
            &agent_execution_id,
            &target,
            &state,
        )
        .await?;

        let tool_counts = json!({"read_file": 1, "fs.read_text_file": 1});
        let (status, calls, successes, failures, persisted_tool_counts, persisted_metadata) =
            load_agent_tool_audit(&ctx.engine.pool, &agent_execution_id).await?;
        ensure!(
            status == "SUCCEEDED" && calls == 1 && successes == 1 && failures == 0,
            "agent tool audit returned incorrect callback counters"
        );
        ensure!(
            persisted_tool_counts == tool_counts,
            "agent tool audit returned incorrect callback tool counts"
        );
        ensure!(
            persisted_metadata["tool_call_audit"]["summary"]["total"] == 1
                && persisted_metadata["tool_call_audit"]["summary"]["callback_count"] == 1
                && persisted_metadata["tool_call_audit"]["summary"]["provider_notification_count"] == 1
                && persisted_metadata["tool_call_audit"]["correlation_capability"] == "SUPPORTED"
                && persisted_metadata["tool_call_audit"]["summary"]["unmatched_provider_calls"] == 0
                && persisted_metadata["tool_call_audit"]["summary"]["unmatched_callbacks"] == 0
                && persisted_metadata["tool_call_audit"]["entries"][0]["terminal_state"]
                    == "SUCCESS"
                && persisted_metadata["tool_call_audit"]["entries"][0]["provider_update_correlation"]
                    == "CORRELATED",
            "agent tool audit returned incorrect callback metadata"
        );
        let report = render_tool_call_audit(&persisted_metadata);
        ensure!(
            report.contains("fs/read_text_file")
                && report.contains("| MATCH |")
                && report.contains("fs.read_text_file")
                && report.contains("SUCCESS")
                && report.contains("oti-fixture-1")
                && report.contains("provider-call-fixture-1")
                && !report.contains("provider-secret"),
            "tool-call audit renderer returned incorrect correlated report"
        );
        Ok(())
    }
    .await;

    finish_test_context(ctx, fixture_result).await
}

fn init_git_repo(path: &Path) -> Result<()> {
    common::init_git_repo(path)
}

// -----------------------------------------------------------------------------
// CHECKPOINTS 1-5: Canonical Identity, Wire Aliases, and Rejection
// -----------------------------------------------------------------------------

#[test]
fn canonical_identity_resolution_all_18_tools() -> Result<()> {
    let canonical_tools = [
        (
            CanonicalToolName::FsReadTextFile,
            "fs.read_text_file",
            "read_file",
        ),
        (
            CanonicalToolName::FsWriteTextFile,
            "fs.write_text_file",
            "write_file",
        ),
        (CanonicalToolName::FsEditFile, "fs.edit_file", "edit_file"),
        (
            CanonicalToolName::FsListDirectory,
            "fs.list_directory",
            "list_directory",
        ),
        (CanonicalToolName::FsFindPath, "fs.find_path", "find_path"),
        (
            CanonicalToolName::FsCreateDirectory,
            "fs.create_directory",
            "create_directory",
        ),
        (CanonicalToolName::FsMove, "fs.move", "move"),
        (CanonicalToolName::FsCopy, "fs.copy", "copy"),
        (
            CanonicalToolName::FsDeleteFile,
            "fs.delete_file",
            "delete_file",
        ),
        (
            CanonicalToolName::FsDeleteDirectory,
            "fs.delete_directory",
            "delete_directory",
        ),
        (CanonicalToolName::SearchGrep, "search.grep", "grep"),
        (
            CanonicalToolName::TerminalCreate,
            "terminal.create",
            "shell",
        ),
        (
            CanonicalToolName::TerminalOutput,
            "terminal.output",
            "terminal/output",
        ),
        (
            CanonicalToolName::TerminalWaitForExit,
            "terminal.wait_for_exit",
            "terminal/wait_for_exit",
        ),
        (
            CanonicalToolName::TerminalKill,
            "terminal.kill",
            "terminal/kill",
        ),
        (
            CanonicalToolName::TerminalRelease,
            "terminal.release",
            "terminal/release",
        ),
        (CanonicalToolName::GitStatus, "git.status", "git_status"),
        (CanonicalToolName::GitDiff, "git.diff", "git_diff"),
        (CanonicalToolName::GitShow, "git.show", "git_show"),
    ];

    assert_eq!(canonical_tools.len(), 19);

    for (tool, canonical_str, legacy_str) in canonical_tools {
        assert_eq!(tool.as_str(), canonical_str);
        assert_eq!(tool.legacy_name(), legacy_str);
        assert_eq!(CanonicalToolName::from_canonical(canonical_str), Some(tool));
        assert_eq!(CanonicalToolName::from_wire(canonical_str), Some(tool));
    }

    // Verify all tool definitions exist and have complete schemas
    let tool_names = [
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
        "git_status",
        "git_diff",
        "git_show",
    ];
    let names_vec: Vec<String> = tool_names.iter().map(|s| s.to_string()).collect();
    let defs = coding_agent::tool_definitions(&names_vec)?;
    for tool_name in tool_names {
        let def = defs.iter().find(|d| d["name"] == tool_name);
        assert!(def.is_some(), "missing tool definition for {tool_name}");
        let def = def.unwrap();
        assert!(!def["description"].as_str().unwrap().is_empty());
        assert!(def["parameters"].is_object());
    }

    Ok(())
}

#[test]
fn wire_alias_and_provider_routing() -> Result<()> {
    // ACP slash aliases
    assert_eq!(
        CanonicalToolName::from_wire("fs/read_text_file"),
        Some(CanonicalToolName::FsReadTextFile)
    );
    assert_eq!(
        CanonicalToolName::from_wire("fs/write_text_file"),
        Some(CanonicalToolName::FsWriteTextFile)
    );
    assert_eq!(
        CanonicalToolName::from_wire("fs/edit_file"),
        Some(CanonicalToolName::FsEditFile)
    );
    assert_eq!(
        CanonicalToolName::from_wire("fs/list_directory"),
        Some(CanonicalToolName::FsListDirectory)
    );
    assert_eq!(
        CanonicalToolName::from_wire("fs/find_path"),
        Some(CanonicalToolName::FsFindPath)
    );
    assert_eq!(
        CanonicalToolName::from_wire("fs/create_directory"),
        Some(CanonicalToolName::FsCreateDirectory)
    );
    assert_eq!(
        CanonicalToolName::from_wire("fs/move"),
        Some(CanonicalToolName::FsMove)
    );
    assert_eq!(
        CanonicalToolName::from_wire("fs/copy"),
        Some(CanonicalToolName::FsCopy)
    );
    assert_eq!(
        CanonicalToolName::from_wire("fs/delete_file"),
        Some(CanonicalToolName::FsDeleteFile)
    );
    assert_eq!(
        CanonicalToolName::from_wire("fs/delete_directory"),
        Some(CanonicalToolName::FsDeleteDirectory)
    );
    assert_eq!(
        CanonicalToolName::from_wire("search/grep"),
        Some(CanonicalToolName::SearchGrep)
    );
    assert_eq!(
        CanonicalToolName::from_wire("terminal/create"),
        Some(CanonicalToolName::TerminalCreate)
    );
    assert_eq!(
        CanonicalToolName::from_wire("terminal/output"),
        Some(CanonicalToolName::TerminalOutput)
    );
    assert_eq!(
        CanonicalToolName::from_wire("terminal/wait_for_exit"),
        Some(CanonicalToolName::TerminalWaitForExit)
    );
    assert_eq!(
        CanonicalToolName::from_wire("terminal/kill"),
        Some(CanonicalToolName::TerminalKill)
    );
    assert_eq!(
        CanonicalToolName::from_wire("terminal/release"),
        Some(CanonicalToolName::TerminalRelease)
    );
    assert_eq!(
        CanonicalToolName::from_wire("git/status"),
        Some(CanonicalToolName::GitStatus)
    );
    assert_eq!(
        CanonicalToolName::from_wire("git/diff"),
        Some(CanonicalToolName::GitDiff)
    );
    assert_eq!(
        CanonicalToolName::from_wire("git/show"),
        Some(CanonicalToolName::GitShow)
    );

    // Provider bare aliases
    assert_eq!(
        CanonicalToolName::from_wire("read_file"),
        Some(CanonicalToolName::FsReadTextFile)
    );
    assert_eq!(
        CanonicalToolName::from_wire("write_file"),
        Some(CanonicalToolName::FsWriteTextFile)
    );
    assert_eq!(
        CanonicalToolName::from_wire("edit_file"),
        Some(CanonicalToolName::FsEditFile)
    );
    assert_eq!(
        CanonicalToolName::from_wire("list_directory"),
        Some(CanonicalToolName::FsListDirectory)
    );
    assert_eq!(
        CanonicalToolName::from_wire("find_path"),
        Some(CanonicalToolName::FsFindPath)
    );
    assert_eq!(
        CanonicalToolName::from_wire("create_directory"),
        Some(CanonicalToolName::FsCreateDirectory)
    );
    assert_eq!(
        CanonicalToolName::from_wire("move"),
        Some(CanonicalToolName::FsMove)
    );
    assert_eq!(
        CanonicalToolName::from_wire("copy"),
        Some(CanonicalToolName::FsCopy)
    );
    assert_eq!(
        CanonicalToolName::from_wire("delete_file"),
        Some(CanonicalToolName::FsDeleteFile)
    );
    assert_eq!(
        CanonicalToolName::from_wire("delete_directory"),
        Some(CanonicalToolName::FsDeleteDirectory)
    );
    assert_eq!(
        CanonicalToolName::from_wire("grep"),
        Some(CanonicalToolName::SearchGrep)
    );
    assert_eq!(
        CanonicalToolName::from_wire("shell"),
        Some(CanonicalToolName::TerminalCreate)
    );
    assert_eq!(
        CanonicalToolName::from_wire("git_status"),
        Some(CanonicalToolName::GitStatus)
    );
    assert_eq!(
        CanonicalToolName::from_wire("git_diff"),
        Some(CanonicalToolName::GitDiff)
    );
    assert_eq!(
        CanonicalToolName::from_wire("git_show"),
        Some(CanonicalToolName::GitShow)
    );

    // Bridge orbit_* aliases
    assert_eq!(
        CanonicalToolName::from_wire("orbit_read"),
        Some(CanonicalToolName::FsReadTextFile)
    );
    assert_eq!(
        CanonicalToolName::from_wire("orbit_write"),
        Some(CanonicalToolName::FsWriteTextFile)
    );
    assert_eq!(
        CanonicalToolName::from_wire("orbit_edit"),
        Some(CanonicalToolName::FsEditFile)
    );
    assert_eq!(
        CanonicalToolName::from_wire("orbit_list"),
        Some(CanonicalToolName::FsListDirectory)
    );
    assert_eq!(
        CanonicalToolName::from_wire("orbit_find"),
        Some(CanonicalToolName::FsFindPath)
    );
    assert_eq!(
        CanonicalToolName::from_wire("orbit_grep"),
        Some(CanonicalToolName::SearchGrep)
    );
    assert_eq!(
        CanonicalToolName::from_wire("orbit_terminal"),
        Some(CanonicalToolName::TerminalCreate)
    );

    Ok(())
}

#[tokio::test]
async fn unsupported_tool_rejection() -> Result<()> {
    let repo = tempdir()?;
    let (server_in, client_out) = tokio::io::duplex(65536);
    let (client_in, server_out) = tokio::io::duplex(65536);
    let mut server_wire = Wire::new(server_in, server_out, 16 * 1024 * 1024);
    let mut client_wire = Wire::new(client_in, client_out, 16 * 1024 * 1024);

    let mut state = AcpTurnState::new(repo.path(), WorkspaceAccess::ReadWrite);

    let unknown_tools = ["arbitrary_code_exec", "fs/magic", "system_reboot", "eval"];
    for (i, t) in unknown_tools.iter().enumerate() {
        let msg = json!({
            "jsonrpc": "2.0",
            "id": i + 1,
            "method": t,
            "params": {}
        });
        handle_acp_message(&mut server_wire, &mut state, msg).await?;
        let resp = client_wire.read().await?;
        assert!(resp.get("error").is_some());
        let err = &resp["error"];
        assert_eq!(err["code"].as_i64(), Some(-32601));
        assert!(
            err["message"]
                .as_str()
                .unwrap()
                .contains(ERR_UNSUPPORTED_TOOL)
        );
    }

    Ok(())
}

// -----------------------------------------------------------------------------
// CHECKPOINTS 6-19: Role Matrix Permission Enforcement
// -----------------------------------------------------------------------------

#[tokio::test]
async fn role_matrix_planner_denial_and_implementer_allowance() -> Result<()> {
    let repo = tempdir()?;
    let (server_in, client_out) = tokio::io::duplex(65536);
    let (client_in, server_out) = tokio::io::duplex(65536);
    let mut server_wire = Wire::new(server_in, server_out, 16 * 1024 * 1024);
    let mut client_wire = Wire::new(client_in, client_out, 16 * 1024 * 1024);

    // Initial repo file
    fs::write(repo.path().join("file.txt"), "hello initial")?;

    // 1. Planner (ReadOnly): Mutating tools must be denied
    let mut read_only_state = AcpTurnState::new(repo.path(), WorkspaceAccess::ReadOnly);
    read_only_state.role_id = Some("planner".into());
    read_only_state.workspace_identity =
        Some(repo.path().canonicalize()?.to_string_lossy().into_owned());
    let mutating_methods = [
        (
            "fs/write_text_file",
            json!({ "path": "test.txt", "content": "data" }),
        ),
        (
            "fs/edit_file",
            json!({ "path": "file.txt", "old_text": "hello", "new_text": "world" }),
        ),
        ("fs/create_directory", json!({ "path": "new_dir" })),
        (
            "fs/move",
            json!({ "source": "file.txt", "destination": "file2.txt" }),
        ),
        (
            "fs/copy",
            json!({ "source": "file.txt", "destination": "copy.txt" }),
        ),
        ("fs/delete_file", json!({ "path": "file.txt" })),
        ("fs/delete_directory", json!({ "path": "new_dir" })),
        (
            "terminal/create",
            json!({ "command": "echo", "args": ["hi"] }),
        ),
    ];

    for (i, (method, params)) in mutating_methods.iter().enumerate() {
        let msg = json!({
            "jsonrpc": "2.0",
            "id": i + 1,
            "method": method,
            "params": params
        });
        handle_acp_message(&mut server_wire, &mut read_only_state, msg).await?;
        let resp = client_wire.read().await?;
        assert!(
            resp.get("error").is_some(),
            "expected error for {method} under ReadOnly"
        );
        let msg_str = resp["error"]["message"].as_str().unwrap();
        assert!(
            msg_str.contains("read-only")
                || msg_str.contains(ERR_READ_ONLY_ROLE)
                || (*method == "terminal/create"
                    && msg_str.contains("CLI_WORKFLOW_TERMINAL_DISABLED")),
            "expected read-only denial error for {method}, got: {msg_str}"
        );
    }

    // 2. Planner (ReadOnly): Inspection tools must succeed
    let inspection_methods = [
        ("fs/read_text_file", json!({ "path": "file.txt" })),
        ("fs/list_directory", json!({ "path": "." })),
        ("fs/find_path", json!({ "pattern": "*" })),
        ("search/grep", json!({ "query": "hello" })),
    ];

    for (i, (method, params)) in inspection_methods.iter().enumerate() {
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 100 + i,
            "method": method,
            "params": params
        });
        handle_acp_message(&mut server_wire, &mut read_only_state, msg).await?;
        let resp = client_wire.read().await?;
        assert!(
            resp.get("result").is_some(),
            "expected success for {method} under ReadOnly, got: {resp:?}"
        );
    }

    // 3. ToolMetadata role check
    for tool in [
        CanonicalToolName::FsReadTextFile,
        CanonicalToolName::FsListDirectory,
        CanonicalToolName::FsFindPath,
        CanonicalToolName::SearchGrep,
        CanonicalToolName::GitStatus,
        CanonicalToolName::GitDiff,
        CanonicalToolName::GitShow,
    ] {
        let meta = ToolMetadata::for_tool(tool);
        assert!(meta.allowed_roles.contains(&"planner".to_string()));
        assert!(meta.allowed_roles.contains(&"implementer".to_string()));
        assert!(meta.allowed_roles.contains(&"reviewer".to_string()));
    }

    for tool in [
        CanonicalToolName::FsWriteTextFile,
        CanonicalToolName::FsEditFile,
        CanonicalToolName::FsCreateDirectory,
        CanonicalToolName::FsMove,
        CanonicalToolName::FsCopy,
        CanonicalToolName::FsDeleteFile,
        CanonicalToolName::FsDeleteDirectory,
        CanonicalToolName::TerminalCreate,
    ] {
        let meta = ToolMetadata::for_tool(tool);
        assert!(!meta.allowed_roles.contains(&"planner".to_string()));
        assert!(meta.allowed_roles.contains(&"implementer".to_string()));
        assert!(!meta.allowed_roles.contains(&"reviewer".to_string()));
    }

    Ok(())
}

// -----------------------------------------------------------------------------
// CHECKPOINTS 20-21: Mutation Lock Enforcement
// -----------------------------------------------------------------------------

#[tokio::test]
async fn missing_mutation_lock_context_is_denied() -> Result<()> {
    let repo = tempdir()?;
    let (server_in, client_out) = tokio::io::duplex(65536);
    let (client_in, server_out) = tokio::io::duplex(65536);
    let mut server_wire = Wire::new(server_in, server_out, 16 * 1024 * 1024);
    let mut client_wire = Wire::new(client_in, client_out, 16 * 1024 * 1024);
    let mut state = AcpTurnState::new(repo.path(), WorkspaceAccess::ReadWrite);
    state.role_id = Some("implementer".into());
    state.workspace_identity = Some(repo.path().canonicalize()?.to_string_lossy().into_owned());

    handle_acp_message(
        &mut server_wire,
        &mut state,
        json!({
            "jsonrpc":"2.0", "id":1, "method":"fs/write_text_file",
            "params":{"path":"must-not-exist.txt", "content":"denied"}
        }),
    )
    .await?;

    let response = client_wire.read().await?;
    assert!(response.get("error").is_some());
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains(ERR_MUTATION_LOCK_REQUIRED)
    );
    assert!(!repo.path().join("must-not-exist.txt").exists());
    Ok(())
}

#[tokio::test]
async fn dispatch_enforces_reviewer_identity_call_and_output_limits() -> Result<()> {
    let repo = tempdir()?;
    let file_content = "e\u{301}\n".repeat(30_000);
    fs::write(repo.path().join("large.txt"), &file_content)?;
    let (server_in, client_out) = tokio::io::duplex(131_072);
    let (client_in, server_out) = tokio::io::duplex(131_072);
    let mut server_wire = Wire::new(server_in, server_out, 16 * 1024 * 1024);
    let mut client_wire = Wire::new(client_in, client_out, 16 * 1024 * 1024);
    let mut state = AcpTurnState::new(repo.path(), WorkspaceAccess::ReadOnly);
    state.role_id = Some("reviewer".into());
    state.workspace_identity = Some(repo.path().canonicalize()?.to_string_lossy().into_owned());

    handle_acp_message(
        &mut server_wire,
        &mut state,
        json!({
            "jsonrpc":"2.0", "id":1, "method":"fs/write_text_file",
            "params":{"path":"denied.txt", "content":"bad"}
        }),
    )
    .await?;
    let response = client_wire.read().await?;
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains(ERR_READ_ONLY_ROLE)
    );
    assert!(!repo.path().join("denied.txt").exists());

    state.workspace_identity = None;
    handle_acp_message(
        &mut server_wire,
        &mut state,
        json!({
            "jsonrpc":"2.0", "id":2, "method":"fs/read_text_file",
            "params":{"path":"large.txt"}
        }),
    )
    .await?;
    let response = client_wire.read().await?;
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("WORKSPACE_IDENTITY_REQUIRED")
    );

    state.workspace_identity = Some(repo.path().canonicalize()?.to_string_lossy().into_owned());
    let mut line = 1u32;
    let mut request_id = 3u64;
    let mut assembled = String::new();
    loop {
        let mut params = json!({"path":"large.txt"});
        params["line"] = json!(line);
        if request_id == 3 {
            params["limit"] = json!(25_000);
        }
        handle_acp_message(
            &mut server_wire,
            &mut state,
            json!({
                "jsonrpc":"2.0", "id":request_id, "method":"fs/read_text_file",
                "params":params
            }),
        )
        .await?;
        let response = client_wire.read().await?;
        let result = response
            .get("result")
            .context("large file read returned a JSON-RPC error")?;
        assert!(serde_json::to_vec(result)?.len() <= 65536);
        let metadata = &result["_meta"]["orbit"];
        assert_eq!(metadata["line"].as_u64(), Some(u64::from(line)));
        assert_eq!(
            metadata["total_bytes"].as_u64(),
            Some(file_content.len() as u64)
        );
        assembled.push_str(
            result["content"]
                .as_str()
                .context("read page omitted content")?,
        );

        match metadata["next_line"].as_u64() {
            Some(next_line) => {
                assert!(metadata["truncated"].as_bool().unwrap_or(false));
                assert!(next_line > u64::from(line));
                assert!(result["content"].as_str().unwrap().ends_with('\n'));
                line = u32::try_from(next_line)?;
                request_id += 1;
            }
            None => {
                assert_eq!(metadata["truncated"], false);
                break;
            }
        }
    }
    assert_eq!(assembled, file_content);

    let invalid_range_id = request_id + 1;
    handle_acp_message(
        &mut server_wire,
        &mut state,
        json!({
            "jsonrpc":"2.0", "id":invalid_range_id, "method":"fs/read_text_file",
            "params":{"path":"large.txt", "line":0}
        }),
    )
    .await?;
    let response = client_wire.read().await?;
    assert_eq!(response["error"]["message"], "INVALID_REQUEST");

    fs::write(repo.path().join("long-line.txt"), "x".repeat(70_000))?;
    handle_acp_message(
        &mut server_wire,
        &mut state,
        json!({
            "jsonrpc":"2.0", "id":invalid_range_id + 1, "method":"fs/read_text_file",
            "params":{"path":"long-line.txt"}
        }),
    )
    .await?;
    let response = client_wire.read().await?;
    assert_eq!(response["error"]["message"], "OUTPUT_LIMIT");

    state.tool_call_limit = state.tool_calls;
    handle_acp_message(
        &mut server_wire,
        &mut state,
        json!({
            "jsonrpc":"2.0", "id":invalid_range_id + 2, "method":"fs/read_text_file",
            "params":{"path":"large.txt"}
        }),
    )
    .await?;
    let response = client_wire.read().await?;
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("TOOL_CALL_LIMIT")
    );
    Ok(())
}

#[tokio::test]
async fn cli_terminal_create_is_denied() -> Result<()> {
    let repo = tempdir()?;
    let (server_in, client_out) = tokio::io::duplex(65536);
    let (client_in, server_out) = tokio::io::duplex(65536);
    let mut server_wire = Wire::new(server_in, server_out, 16 * 1024 * 1024);
    let mut client_wire = Wire::new(client_in, client_out, 16 * 1024 * 1024);
    let mut state = AcpTurnState::new(repo.path(), WorkspaceAccess::ReadWrite);

    handle_acp_message(
        &mut server_wire,
        &mut state,
        json!({
            "jsonrpc":"2.0", "id":1, "method":"terminal/create",
            "params":{"command":"sh", "args":["-c", "touch terminal-ran"]}
        }),
    )
    .await?;

    let response = client_wire.read().await?;
    assert!(response.get("error").is_some());
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("CLI_WORKFLOW_TERMINAL_DISABLED")
    );
    assert!(!repo.path().join("terminal-ran").exists());
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn attempt_mutation_lock_enforcement() -> Result<()> {
    let ctx = setup_test().await?;

    let codex_thread = orbit::codex_bridge::thread_start(
        orbit::codex_bridge::CODEX_VERSION,
        "fixture-model",
        None,
        Path::new("/orbit/home"),
        &["read_file".into(), "write_file".into()],
    )?;
    assert_eq!(codex_thread["sandbox"], "read-only");
    assert_eq!(codex_thread["dynamicTools"][1]["name"], "orbit_write_file");
    assert!(
        codex_thread["baseInstructions"]
            .as_str()
            .unwrap()
            .contains("Orbit's authorized write path")
    );

    let repo = tempdir()?;
    let (server_in, client_out) = tokio::io::duplex(65536);
    let (client_in, server_out) = tokio::io::duplex(65536);
    let mut server_wire = Wire::new(server_in, server_out, 16 * 1024 * 1024);
    let mut client_wire = Wire::new(client_in, client_out, 16 * 1024 * 1024);

    let att_id = format!("att-{}", id());
    let wf = ctx
        .store
        .create_workflow_run_full(
            "software_change_v1",
            &att_id,
            3,
            None,
            None,
            None,
            Some("mutation lock test"),
            Some(repo.path().to_str().unwrap()),
            None,
        )
        .await?;

    let role = RoleDefinition::implementer_v1();
    let role_exec = ctx
        .store
        .create_role_execution(&wf.id, &role, "IMPLEMENTING", 0, None, None)
        .await?;
    let (agent_execution_id, target) = start_fixture_agent_execution(&ctx, &role_exec).await?;

    let mut state = AcpTurnState::new(repo.path(), WorkspaceAccess::ReadWrite);
    state.role_id = Some("implementer".into());
    state.workspace_identity = Some(repo.path().canonicalize()?.to_string_lossy().into_owned());
    state.wf_attempt_id = Some(att_id.clone());
    state.role_exec_id = Some(role_exec.id.clone());
    state.agent_exec_id = Some(agent_execution_id.clone());
    state.pool = Some(&ctx.engine.pool);

    // 1. Without lock: mutating call denied with ERR_MUTATION_LOCK_REQUIRED
    let msg1 = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "fs/write_text_file",
        "params": {
            "path": "test.txt",
            "content": "payload"
        }
    });
    let resp1 =
        dispatch_correlated_fixture_callback(&mut server_wire, &mut client_wire, &mut state, msg1)
            .await?;
    assert!(resp1.get("error").is_some());
    assert!(
        resp1["error"]["message"]
            .as_str()
            .unwrap()
            .contains(ERR_MUTATION_LOCK_REQUIRED)
    );

    // 2. Acquire lock: mutating call succeeds
    ctx.store
        .acquire_workspace_mutation_lock(&att_id, &role_exec.id)
        .await?;

    let msg2 = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "fs/write_text_file",
        "params": {
            "path": "test.txt",
            "content": "payload"
        }
    });
    let resp2 =
        dispatch_correlated_fixture_callback(&mut server_wire, &mut client_wire, &mut state, msg2)
            .await?;
    assert!(resp2.get("result").is_some());
    assert_eq!(fs::read_to_string(repo.path().join("test.txt"))?, "payload");

    finish_fixture_agent_execution(&ctx, &role_exec, &agent_execution_id, &target, &state).await?;

    let (status, calls, successes, failures, _, metadata) =
        load_agent_tool_audit(&ctx.engine.pool, &agent_execution_id).await?;
    ensure!(
        status == "SUCCEEDED" && calls == 2 && successes == 1 && failures == 1,
        "mutation lock fixture did not persist the completed execution counters"
    );
    ensure!(
        metadata["tool_call_audit"]["correlation_capability"] == "SUPPORTED"
            && metadata["tool_call_audit"]["summary"]["unmatched_provider_calls"] == 0
            && metadata["tool_call_audit"]["summary"]["unmatched_callbacks"] == 0,
        "mutation lock fixture did not persist exact callback correlation"
    );

    teardown_test(ctx).await?;
    Ok(())
}

// -----------------------------------------------------------------------------
// CHECKPOINT 22: Path Confinement Security
// -----------------------------------------------------------------------------

#[test]
fn path_confinement_security() -> Result<()> {
    let repo = tempdir()?;
    let repo_path = repo.path();

    // 1. Parent traversal escapes
    assert!(confine_path(repo_path, "../secret.txt", false, false).is_err());
    assert!(confine_path(repo_path, "sub/../../escape.txt", false, false).is_err());

    // 2. Host absolute paths outside workspace
    assert!(confine_path(repo_path, "/etc/passwd", false, false).is_err());
    assert!(confine_path(repo_path, "/tmp/evil", false, false).is_err());

    // 3. Symlink pointing outside workspace
    let outside = tempdir()?;
    let target = outside.path().join("outside.txt");
    fs::write(&target, "secret")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let link_path = repo_path.join("leak_link");
        symlink(&target, &link_path)?;
        assert!(confine_path(repo_path, "leak_link", true, false).is_err());
    }

    // 4. Virtual ACP prefix is accepted and confined
    let virtual_path = "/orbit/home/workspace/src/lib.rs";
    let confined = confine_path(repo_path, virtual_path, false, false)?;
    assert_eq!(confined, repo_path.canonicalize()?.join("src/lib.rs"));

    Ok(())
}

// -----------------------------------------------------------------------------
// CHECKPOINTS 23-27: Filesystem and Search Tool Functionality
// -----------------------------------------------------------------------------

#[test]
fn fs_list_directory_and_find_path() -> Result<()> {
    let repo = tempdir()?;
    let p = repo.path();

    fs::create_dir_all(p.join("src"))?;
    fs::create_dir_all(p.join("docs"))?;
    fs::create_dir_all(p.join(".git"))?;

    fs::write(p.join("src/main.rs"), "fn main() {}")?;
    fs::write(p.join("src/lib.rs"), "pub fn run() {}")?;
    fs::write(p.join("docs/README.md"), "# Readme")?;
    fs::write(p.join(".hidden.txt"), "hidden content")?;

    // list_directory flat
    let flat = list_directory(p, ".", false, 50, false)?;
    let flat_names: Vec<_> = flat.entries.iter().map(|e| e.name.as_str()).collect();
    assert!(flat_names.contains(&"src"));
    assert!(flat_names.contains(&"docs"));
    assert!(!flat_names.contains(&".hidden.txt"));
    assert!(!flat.truncated);

    // list_directory recursive
    let rec = list_directory(p, ".", true, 50, false)?;
    let rec_names: Vec<_> = rec.entries.iter().map(|e| e.name.as_str()).collect();
    assert!(rec_names.contains(&"src/main.rs"));
    assert!(rec_names.contains(&"src/lib.rs"));
    assert!(rec_names.contains(&"docs/README.md"));
    assert!(!rec_names.contains(&".hidden.txt"));

    // list_directory with hidden
    let with_hidden = list_directory(p, ".", false, 50, true)?;
    let hidden_names: Vec<_> = with_hidden
        .entries
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    assert!(hidden_names.contains(&".hidden.txt"));

    // list_directory max_entries cap
    let capped = list_directory(p, ".", true, 2, false)?;
    assert_eq!(capped.entries.len(), 2);
    assert!(capped.truncated);

    // find_path
    let rust_files = find_path(p, None, "*.rs", &[], &[], 10)?;
    assert_eq!(rust_files.matches.len(), 2);

    let doc_files = find_path(p, Some("docs"), "*.md", &[], &[], 10)?;
    assert_eq!(doc_files.matches.len(), 1);
    assert_eq!(doc_files.matches[0].path, "docs/README.md");

    Ok(())
}

#[test]
fn search_grep_functionality() -> Result<()> {
    let repo = tempdir()?;
    let p = repo.path();

    fs::create_dir_all(p.join("src"))?;
    fs::write(
        p.join("src/alpha.rs"),
        "fn calculate_hash() {\n    let val = 42;\n}\n",
    )?;
    fs::write(
        p.join("src/beta.rs"),
        "fn verify_hash() {\n    let val = 100;\n}\n",
    )?;
    fs::write(p.join("src/gamma.txt"), "No hash here.\nJust notes.\n")?;

    // 1. Literal search
    let grep_hash = search_grep(p, None, "hash", true, false, &[], &[], 10, 0)?;
    assert_eq!(grep_hash.matches.len(), 3); // 2 fn lines + 1 gamma line
    assert!(!grep_hash.truncated);

    // 2. Regex search
    let grep_regex = search_grep(p, None, r"fn *_hash()", true, true, &[], &[], 10, 0)?;
    assert_eq!(grep_regex.matches.len(), 2);

    // 3. Case-insensitive search
    let grep_case = search_grep(p, None, "no HASH", false, false, &[], &[], 10, 0)?;
    assert_eq!(grep_case.matches.len(), 1);
    assert_eq!(grep_case.matches[0].line, 1);

    // 4. Max matches cap
    let grep_capped = search_grep(p, None, "hash", true, false, &[], &[], 1, 0)?;
    assert_eq!(grep_capped.matches.len(), 1);
    assert!(grep_capped.truncated);

    Ok(())
}

#[test]
fn fs_edit_file_exact_matching_and_replacements() -> Result<()> {
    let repo = tempdir()?;
    let p = repo.path();
    let file = p.join("config.txt");

    fs::write(&file, "foo = 1\nbar = 2\nfoo = 3\n")?;

    // 1. Missing match -> ERR_NO_MATCH
    let err_missing = edit_file(p, "config.txt", "missing = 0", "present = 1", false);
    assert!(err_missing.is_err());
    assert!(err_missing.unwrap_err().to_string().contains(ERR_NO_MATCH));

    // 2. Ambiguous match when replace_all=false -> ERR_MULTIPLE_MATCHES
    let err_dup = edit_file(p, "config.txt", "foo", "qux", false);
    assert!(err_dup.is_err());
    assert!(
        err_dup
            .unwrap_err()
            .to_string()
            .contains(ERR_MULTIPLE_MATCHES)
    );

    // 3. Single match exact replacement
    let res_single = edit_file(p, "config.txt", "bar = 2", "bar = 99", false)?;
    assert_eq!(res_single.matches_replaced, 1);
    assert_eq!(fs::read_to_string(&file)?, "foo = 1\nbar = 99\nfoo = 3\n");

    // 4. Multi-replacement when replace_all=true
    let res_all = edit_file(p, "config.txt", "foo", "baz", true)?;
    assert_eq!(res_all.matches_replaced, 2);
    assert_eq!(fs::read_to_string(&file)?, "baz = 1\nbar = 99\nbaz = 3\n");

    Ok(())
}

#[test]
fn fs_copy_file_and_directory() -> Result<()> {
    let repo = tempdir()?;
    let p = repo.path();

    let src_file = p.join("src.txt");
    fs::write(&src_file, "original text")?;

    // 1. File copy
    let res = copy_path(p, "src.txt", "dst.txt", false)?;
    assert!(res.success);
    assert_eq!(fs::read_to_string(p.join("dst.txt"))?, "original text");

    // 2. Collision fails closed
    let col = copy_path(p, "src.txt", "dst.txt", false);
    assert!(col.is_err());

    // 3. Recursive directory copy
    fs::create_dir_all(p.join("tree/sub"))?;
    fs::write(p.join("tree/a.txt"), "A")?;
    fs::write(p.join("tree/sub/b.txt"), "B")?;

    let res_tree = copy_path(p, "tree", "tree_copy", true)?;
    assert!(res_tree.success);
    assert_eq!(fs::read_to_string(p.join("tree_copy/a.txt"))?, "A");
    assert_eq!(fs::read_to_string(p.join("tree_copy/sub/b.txt"))?, "B");

    Ok(())
}

// -----------------------------------------------------------------------------
// CHECKPOINTS 28-30: Git Inspection Tools
// -----------------------------------------------------------------------------

#[tokio::test]
async fn git_status_diff_and_show() -> Result<()> {
    let repo = tempdir()?;
    let p = repo.path();
    init_git_repo(p)?;

    // Initial commit
    fs::write(p.join("tracked.txt"), "initial v1\n")?;
    std::process::Command::new("git")
        .args(["add", "tracked.txt"])
        .current_dir(p)
        .output()?;
    std::process::Command::new("git")
        .args(["commit", "-m", "initial commit"])
        .current_dir(p)
        .output()?;

    // Modify tracked file, create untracked file
    fs::write(p.join("tracked.txt"), "initial v1\nmodified line\n")?;
    fs::write(p.join("untracked.txt"), "brand new\n")?;

    // git/status
    let status = git_status(p, None).await?;
    assert!(!status.clean);
    assert!(status.modified.iter().any(|f| f.contains("tracked.txt")));
    assert!(status.untracked.iter().any(|f| f.contains("untracked.txt")));

    // git/diff
    let diff = git_diff(p, None, None, None, false, 65536).await?;
    assert!(diff.diff.contains("+modified line"));

    let diff_stat = git_diff(p, None, None, None, true, 65536).await?;
    assert!(diff_stat.diff.contains("tracked.txt") || !diff_stat.diff.is_empty());

    // git/show
    let show = git_show(p, "HEAD", None, 65536).await?;
    assert!(show.content.contains("initial commit"));

    let show_file = git_show(p, "HEAD", Some("tracked.txt"), 65536).await?;
    assert_eq!(show_file.content.trim(), "initial v1");

    Ok(())
}

// -----------------------------------------------------------------------------
// CHECKPOINTS 31-35: Terminal Lifecycle and Bounded Preview
// -----------------------------------------------------------------------------

#[tokio::test]
async fn terminal_lifecycle_and_bounded_preview() -> Result<()> {
    let temp = tempdir()?;
    let cwd = temp.path();

    // 1. Success exit 0 with captured stdout
    let term1 = AgentTerminal::spawn(
        cwd,
        "sh",
        &["-c".into(), "echo lifecycle test".into()],
        1024,
    )?;
    let code1 = term1.wait_for_exit(Duration::from_secs(5)).await?;
    assert_eq!(code1, 0);
    let out1 = term1.output();
    assert!(out1.text().contains("lifecycle test"));
    assert!(!out1.truncated);
    assert_eq!(out1.exit_code, Some(0));

    // 2. Command failure with non-zero exit and captured stderr
    let term2 = AgentTerminal::spawn(
        cwd,
        "sh",
        &["-c".into(), "echo err msg >&2; exit 42".into()],
        1024,
    )?;
    let code2 = term2.wait_for_exit(Duration::from_secs(5)).await?;
    assert_eq!(code2, 42);
    let out2 = term2.output();
    assert!(out2.text().contains("err msg"));
    assert_eq!(out2.exit_code, Some(42));

    // 3. Kill running process
    let term3 = AgentTerminal::spawn(cwd, "sleep", &["60".into()], 1024)?;
    term3.kill().await?;
    let out3 = term3.output();
    assert!(out3.exit_code.is_some());

    // 4. Output preview truncation bound (<= 64 KiB)
    let term4 = AgentTerminal::spawn(
        cwd,
        "sh",
        &["-c".into(), "yes overflow | head -n 500".into()],
        100,
    )?;
    let code4 = term4.wait_for_exit(Duration::from_secs(5)).await?;
    assert_eq!(code4, 0);
    let out4 = term4.output();
    assert!(out4.truncated);
    assert!(out4.bytes.len() <= 100);
    assert!(out4.total_bytes > 100);

    Ok(())
}

// -----------------------------------------------------------------------------
// CHECKPOINT: Coordinator Wire Dispatch for All Tools
// -----------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn coordinator_wire_dispatch_enforces_cli_workflow_gates() -> Result<()> {
    let ctx = setup_test().await?;
    let repo = tempdir()?;
    let p = repo.path();
    init_git_repo(p)?;

    let attempt_id = format!("att-{}", id());
    let repo_path = p.to_string_lossy().into_owned();
    let workflow = ctx
        .store
        .create_workflow_run_full(
            "software_change_v1",
            &attempt_id,
            3,
            None,
            None,
            None,
            Some("wire dispatch authority test"),
            Some(&repo_path),
            Some("HEAD"),
        )
        .await?;
    let role = RoleDefinition::implementer_v1();
    let role_execution = ctx
        .store
        .create_role_execution(&workflow.id, &role, "IMPLEMENTING", 0, None, None)
        .await?;
    let (agent_execution_id, target) = start_fixture_agent_execution(&ctx, &role_execution).await?;
    ctx.store
        .acquire_workspace_mutation_lock(&attempt_id, &role_execution.id)
        .await?;

    let (server_in, client_out) = tokio::io::duplex(65536);
    let (client_in, server_out) = tokio::io::duplex(65536);
    let mut server_wire = Wire::new(server_in, server_out, 16 * 1024 * 1024);
    let mut client_wire = Wire::new(client_in, client_out, 16 * 1024 * 1024);

    let mut state = AcpTurnState::new(p, WorkspaceAccess::ReadWrite);
    state.role_id = Some("implementer".into());
    state.workspace_identity = Some(p.canonicalize()?.to_string_lossy().into_owned());
    state.wf_attempt_id = Some(attempt_id.clone());
    state.role_exec_id = Some(role_execution.id.clone());
    state.agent_exec_id = Some(agent_execution_id.clone());
    state.pool = Some(&ctx.engine.pool);

    // 1. fs/create_directory
    let resp = dispatch_correlated_fixture_callback(
        &mut server_wire,
        &mut client_wire,
        &mut state,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "fs/create_directory",
            "params": { "path": "docs" }
        }),
    )
    .await?;
    assert!(resp.get("result").is_some());

    // 2. fs/write_text_file
    let resp = dispatch_correlated_fixture_callback(
        &mut server_wire,
        &mut client_wire,
        &mut state,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "fs/write_text_file",
            "params": { "path": "docs/README.md", "content": "# Initial Docs\nVersion 1.0\n" }
        }),
    )
    .await?;
    assert!(resp.get("result").is_some());

    // 3. fs/edit_file
    let resp = dispatch_correlated_fixture_callback(
        &mut server_wire,
        &mut client_wire,
        &mut state,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "fs/edit_file",
            "params": { "path": "docs/README.md", "old_text": "Version 1.0", "new_text": "Version 2.0" }
        }),
    )
    .await?;
    assert!(resp.get("result").is_some());

    // 4. fs/read_text_file
    let resp = dispatch_correlated_fixture_callback(
        &mut server_wire,
        &mut client_wire,
        &mut state,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "fs/read_text_file",
            "params": { "path": "docs/README.md" }
        }),
    )
    .await?;
    assert!(
        resp["result"]["content"]
            .as_str()
            .unwrap()
            .contains("Version 2.0")
    );

    // 5. fs/copy
    let resp = dispatch_correlated_fixture_callback(
        &mut server_wire,
        &mut client_wire,
        &mut state,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "fs/copy",
            "params": { "source": "docs/README.md", "destination": "docs/README_COPY.md" }
        }),
    )
    .await?;
    assert!(resp.get("result").is_some());

    // 6. fs/list_directory
    let resp = dispatch_correlated_fixture_callback(
        &mut server_wire,
        &mut client_wire,
        &mut state,
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "fs/list_directory",
            "params": { "path": "docs" }
        }),
    )
    .await?;
    assert!(resp.get("result").is_some());

    // 7. fs/find_path
    let resp = dispatch_correlated_fixture_callback(
        &mut server_wire,
        &mut client_wire,
        &mut state,
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "fs/find_path",
            "params": { "pattern": "*.md" }
        }),
    )
    .await?;
    assert!(resp.get("result").is_some());

    // 8. search/grep
    let resp = dispatch_correlated_fixture_callback(
        &mut server_wire,
        &mut client_wire,
        &mut state,
        json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "search/grep",
            "params": { "query": "Initial Docs" }
        }),
    )
    .await?;
    assert!(resp.get("result").is_some());

    // 9. git/status
    let resp = dispatch_correlated_fixture_callback(
        &mut server_wire,
        &mut client_wire,
        &mut state,
        json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "git/status",
            "params": {}
        }),
    )
    .await?;
    assert!(resp.get("result").is_some());

    // 10. terminal/create is always denied until the CLI has a confined owner.
    let terminal_marker = p.join("terminal-must-not-run");
    let resp = dispatch_correlated_fixture_callback(
        &mut server_wire,
        &mut client_wire,
        &mut state,
        json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "terminal/create",
            "params": {
                "command": "sh",
                "args": ["-c", format!("touch {}", terminal_marker.display())]
            }
        }),
    )
    .await?;
    assert!(resp.get("error").is_some());
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("CLI_WORKFLOW_TERMINAL_DISABLED")
    );
    assert!(!terminal_marker.exists());

    // Verify turn state metrics
    assert!(state.tool_calls >= 10);
    assert_eq!(state.tool_failures, 1);
    assert!(state.tool_counts.contains_key("fs.write_text_file"));
    assert!(state.tool_counts.contains_key("fs.edit_file"));
    assert!(state.tool_counts.contains_key("terminal.create"));

    ctx.store
        .release_workspace_mutation_lock(&attempt_id, &role_execution.id)
        .await?;
    finish_fixture_agent_execution(&ctx, &role_execution, &agent_execution_id, &target, &state)
        .await?;

    let (status, calls, successes, failures, _, metadata) =
        load_agent_tool_audit(&ctx.engine.pool, &agent_execution_id).await?;
    ensure!(
        status == "SUCCEEDED" && calls == 10 && successes == 9 && failures == 1,
        "wire dispatch fixture did not persist the expected execution counters"
    );
    ensure!(
        metadata["tool_call_audit"]["correlation_capability"] == "SUPPORTED"
            && metadata["tool_call_audit"]["summary"]["provider_notification_count"] == 10
            && metadata["tool_call_audit"]["summary"]["callback_count"] == 10
            && metadata["tool_call_audit"]["summary"]["unmatched_provider_calls"] == 0
            && metadata["tool_call_audit"]["summary"]["unmatched_callbacks"] == 0,
        "wire dispatch fixture did not persist exact invocation correlation"
    );
    ensure!(
        metadata["tool_call_audit"]["entries"]
            .as_array()
            .is_some_and(|entries| entries.iter().all(|entry| {
                entry["provider_update_correlation"] == "CORRELATED"
                    && entry["terminal_state"] != "UNRESOLVED"
            })),
        "wire dispatch fixture contains an unresolved tool invocation"
    );
    teardown_test(ctx).await?;
    Ok(())
}

#[cfg(feature = "fault-injection")]
#[tokio::test]
#[ignore = "requires disposable PostgreSQL; run with ORBIT_TEST_DATABASE_URL"]
async fn real_acp_execution_row_survives_credential_resolution_failure() -> Result<()> {
    let ctx = setup_test().await?;
    let fixture_result = async {
        let repo = tempdir()?;
        let repo_path = repo.path().canonicalize()?.to_string_lossy().into_owned();
        let attempt_id = format!("att-{}", id());
        let workflow = ctx
            .store
            .create_workflow_run_full(
                "software_change_v1",
                &attempt_id,
                1,
                None,
                None,
                None,
                Some("exercise durable early ACP failure evidence"),
                Some(&repo_path),
                None,
            )
            .await?;
        let role = RoleDefinition::implementer_v1();
        let role_execution = ctx
            .store
            .create_role_execution(&workflow.id, &role, "IMPLEMENTING", 0, None, None)
            .await?;
        let target = ResolvedExecutionTarget {
            provider: "codex".into(),
            runtime_interface: "codex-acp".into(),
            credential_id: Some("synthetic-missing-account".into()),
            credential_generation: Some(7),
            requested_model: Some("gpt-6-luna".into()),
            resolved_model: Some("gpt-6-luna".into()),
            runtime_image_digest: None,
            resolution_reason: "synthetic deterministic test target".into(),
        };
        ctx.store
            .set_role_execution_resolved(&role_execution.id, &target)
            .await?;
        let running_role = ctx
            .store
            .get_role_execution(&role_execution.id)
            .await?
            .context("early-failure RoleExecution was not persisted")?;
        ensure!(
            running_role.status == RoleExecutionStatus::Running
                && running_role.resolved_target.as_ref() == Some(&target),
            "early-failure fixture did not complete normal target resolution"
        );
        let error = match RealAcpRoleExecutor
            .execute_role_with_credential_catalog(
                &ctx.engine.pool,
                &ctx.engine.pool,
                &workflow,
                &role_execution,
                &role,
                &target,
                "A deterministic prompt that fails before runtime startup.",
                repo.path(),
                None,
                tokio::sync::watch::channel(false).1,
            )
            .await
        {
            Ok(_) => anyhow::bail!(
                "synthetic credential is absent, so execution must fail before runtime"
            ),
            Err(error) => error,
        };
        ensure!(
            error.to_string() == "CREDENTIAL_RESOLUTION_FAILED",
            "early execution failure was not preserved as the normalized credential-resolution reason"
        );

        let row = sqlx::query(
            "SELECT id, status, provider, requested_model, resolved_model, actual_model, termination_reason, tool_call_count, tool_success_count, tool_failure_count, metadata FROM orbit_agent_executions WHERE role_execution_id = $1",
        )
        .bind(&role_execution.id)
        .fetch_one(&ctx.engine.pool)
        .await?;
        let agent_execution_id: String = row.try_get("id")?;
        let metadata: serde_json::Value = row.try_get("metadata")?;
        let linked_role = ctx
            .store
            .get_role_execution(&role_execution.id)
            .await?
            .context("early-failure RoleExecution disappeared")?;
        ensure!(
            linked_role.agent_execution_ids.contains(&agent_execution_id),
            "early-failure AgentExecution was not linked to its RoleExecution"
        );
        ensure!(row.try_get::<String, _>("status")? == "FAILED", "unexpected AE status");
        ensure!(row.try_get::<String, _>("provider")? == "codex", "provider was not persisted");
        ensure!(
            row.try_get::<String, _>("requested_model")? == "gpt-6-luna",
            "requested model was not persisted"
        );
        ensure!(
            row.try_get::<String, _>("resolved_model")? == "gpt-6-luna",
            "resolved model was not persisted"
        );
        ensure!(row.try_get::<Option<String>, _>("actual_model")?.is_none(), "actual model must remain unobserved");
        ensure!(
            row.try_get::<Option<String>, _>("termination_reason")?.as_deref()
                == Some("CREDENTIAL_RESOLUTION_FAILED"),
            "normalized early failure reason was not persisted"
        );
        ensure!(row.try_get::<i64, _>("tool_call_count")? == 0, "unexpected tool calls");
        ensure!(row.try_get::<i64, _>("tool_success_count")? == 0, "unexpected successful tool calls");
        ensure!(row.try_get::<i64, _>("tool_failure_count")? == 0, "unexpected failed tool calls");
        ensure!(metadata["account_reference"] == "synthetic-missing-account", "account reference was not persisted");
        ensure!(metadata["credential_generation"] == 7, "credential generation was not persisted");
        ensure!(metadata["expected_runtime_identity"] == "codex-acp", "runtime identity was not persisted");
        ensure!(
            metadata["expected_runtime_profile"]
                == orbit::codex_credential_enrollment::CODEX_IMAGE,
            "expected runtime profile was not persisted"
        );
        ensure!(metadata["lifecycle"]["phase"] == "TERMINAL", "lifecycle was not finalized");
        ensure!(
            metadata["lifecycle"]["last_confirmed_phase"] == "AGENT_EXECUTION_CREATED",
            "last confirmed phase was not preserved"
        );
        ensure!(
            metadata["lifecycle"]["attempted_phase"] == "CREDENTIAL_RESOLUTION",
            "attempted phase was not preserved"
        );
        ensure!(
            metadata["lifecycle"]["failed_phase"] == "CREDENTIAL_RESOLUTION",
            "failed phase was not recorded"
        );
        ensure!(
            metadata["lifecycle"]["normalized_reason"] == "CREDENTIAL_RESOLUTION_FAILED",
            "lifecycle reason was not persisted"
        );
        ensure!(
            metadata["lifecycle"]["cleanup_state"] == "NO_RUNTIME_RESOURCE_CREATED",
            "early failure incorrectly claims runtime cleanup"
        );
        ensure!(
            metadata["lifecycle"]["prompt_uncertainty"] == "NOT_DISPATCHED",
            "prompt state was not preserved"
        );
        ensure!(
            metadata["lifecycle"]["tool_audit_applicability"]
                == "NOT_APPLICABLE_BEFORE_TOOL_PHASE",
            "pre-tool audit applicability was not recorded"
        );
        let entries = metadata["tool_call_audit"]["entries"]
            .as_array()
            .context("tool audit entries missing")?;
        ensure!(entries.is_empty(), "early failure fabricated tool audit rows");

        Ok(())
    }
    .await;
    finish_test_context(ctx, fixture_result).await
}

// -----------------------------------------------------------------------------
// Live Codex and Antigravity role fixtures
// -----------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires explicit live-provider opt-in and a private control-plane URL file"]
#[cfg(feature = "fault-injection")]
async fn real_codex_coding_fixture() -> Result<()> {
    let credential_catalog_pool = explicitly_authorized_live_credential_catalog().await?;
    let ctx = match setup_test().await {
        Ok(ctx) => ctx,
        Err(error) => {
            credential_catalog_pool.close().await;
            return Err(error);
        }
    };
    let mut audit_role_execution_id = None;
    let fixture_result = async {
        ensure_live_catalog_is_separate(
            &credential_catalog_pool,
            &ctx.engine.pool,
            &ctx.database.schema,
        )
        .await?;
        ensure_disposable_schema_has_no_credentials(&ctx.engine.pool).await?;

        let repo = tempdir()?;
        let p = repo.path();
        init_git_repo(p)?;

        fs::create_dir_all(p.join("src"))?;
        fs::write(
            p.join("src/lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        )?;
        fs::write(
            p.join("README.md"),
            "# Sample Project\nAn Orbit coding fixture test.\n",
        )?;
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(p)
            .output()?;
        std::process::Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(p)
            .output()?;

        let candidate_repository_path = p.canonicalize()?.to_string_lossy().into_owned();
        let attempt_id = format!("att-{}", id());
        let created_workflow = ctx
            .store
            .create_workflow_run_full(
                "software_change_v1",
                &attempt_id,
                3,
                None,
                None,
                None,
                Some("Add a multiply function to src/lib.rs and update README.md"),
                Some(&candidate_repository_path),
                None,
            )
            .await?;
        let wf = ctx
            .store
            .get_workflow_run(&created_workflow.id)
            .await?
            .context("Codex fixture workflow was not persisted")?;
        ensure!(
            wf.repository_path.as_deref() == Some(candidate_repository_path.as_str()),
            "Codex fixture workflow is not bound to its temporary repository"
        );

        let role = RoleDefinition::implementer_v1();
        let target =
            resolve_live_target(&credential_catalog_pool, &role, "codex", Some("codex-main"))
                .await?;
        println!(
            "B3.4 live role selection: {}",
            serde_json::to_string(&sanitized_live_selection_summary(&target))?
        );
        let role_exec = ctx
            .store
            .create_role_execution(&wf.id, &role, "IMPLEMENTING", 0, None, None)
            .await?;
        ctx.store
            .set_role_execution_resolved(&role_exec.id, &target)
            .await?;
        let role_exec = ctx
            .store
            .get_role_execution(&role_exec.id)
            .await?
            .context("Codex fixture RoleExecution was not persisted")?;
        ensure!(
            role_exec.status == RoleExecutionStatus::Running,
            "Codex fixture RoleExecution is not RUNNING after target resolution"
        );
        ensure!(
            role_exec.resolved_target.as_ref() == Some(&target),
            "Codex fixture RoleExecution did not persist the selected target"
        );
        audit_role_execution_id = Some(role_exec.id.clone());

        ctx.store
            .acquire_workspace_mutation_lock(&attempt_id, &role_exec.id)
            .await?;

        let outcome = RealAcpRoleExecutor
            .execute_role_with_credential_catalog(
                &ctx.engine.pool,
                &credential_catalog_pool,
                &wf,
                &role_exec,
                &role,
                &target,
                "Add a multiply function to src/lib.rs and document it in README.md",
                p,
                None,
                tokio::sync::watch::channel(false).1,
            )
            .await
            .map_err(|error| anyhow::anyhow!("live provider fixture execution failed: {error}"))?;

        ensure!(
            outcome.raw_output.contains("ORBIT_HANDOFF_START"),
            "implementer did not return a structured handoff"
        );
        ensure!(
            outcome.termination_reason.as_deref() == Some("completed"),
            "implementer fixture did not complete"
        );

        let source = fs::read_to_string(p.join("src/lib.rs"))?;
        ensure!(
            source.contains("pub fn multiply(") && source.contains("a * b"),
            "implementer did not modify src/lib.rs through repository tools"
        );
        let readme = fs::read_to_string(p.join("README.md"))?;
        ensure!(
            readme.to_ascii_lowercase().contains("multiply"),
            "implementer did not update README.md through repository tools"
        );

        let execution_id = outcome
            .agent_execution_ids
            .first()
            .context("implementer execution evidence is missing")?;
        let (
            execution_status,
            tool_call_count,
            tool_success_count,
            tool_failure_count,
            tool_counts,
            execution_metadata,
        ) = load_agent_tool_audit(&ctx.engine.pool, execution_id).await?;
        ensure!(
            execution_status == "SUCCEEDED"
                && tool_failure_count == 0
                && tool_success_count == tool_call_count,
            "implementer execution evidence includes an unsuccessful tool call"
        );
        let tool_audit = &execution_metadata["tool_call_audit"];
        ensure!(
            b34_audit_has_exact_correlations(
                tool_audit,
                usize::try_from(tool_call_count)?,
            ),
            "implementer execution has unsuccessful, omitted, duplicate, or unresolved provider tool calls"
        );
        let file_mutation_calls = ["fs.write_text_file", "fs.edit_file"]
            .iter()
            .filter_map(|tool| tool_counts.get(tool).and_then(serde_json::Value::as_i64))
            .sum::<i64>();
        ensure!(
            file_mutation_calls >= 2,
            "implementer did not record successful repository file mutations"
        );

        let status = git_status(p, None).await?;
        let diff = git_diff(p, None, None, None, false, 65536).await?;
        ensure!(
            !status.clean && !diff.diff.is_empty(),
            "Codex fixture candidate has no repository diff"
        );
        ensure_disposable_schema_has_no_credentials(&ctx.engine.pool).await?;
        Ok(())
    }
    .await;

    print_live_fixture_audit(&ctx.engine.pool, audit_role_execution_id.as_deref()).await;
    finish_live_fixture(credential_catalog_pool, ctx, fixture_result).await
}

#[tokio::test]
#[ignore = "requires explicit live-provider opt-in and a private control-plane URL file"]
#[cfg(feature = "fault-injection")]
async fn real_antigravity_review_fixture() -> Result<()> {
    run_antigravity_role_fixture(false, "reviewer").await
}

#[tokio::test]
#[ignore = "requires explicit live-provider opt-in; candidate remains unqualified"]
#[cfg(feature = "fault-injection")]
async fn real_antigravity_correlated_review_fixture() -> Result<()> {
    run_antigravity_role_fixture(true, "reviewer").await
}

#[tokio::test]
#[ignore = "requires explicit live-provider opt-in; candidate remains unqualified"]
#[cfg(feature = "fault-injection")]
async fn real_antigravity_correlated_planner_fixture() -> Result<()> {
    run_antigravity_role_fixture(true, "planner").await
}

#[tokio::test]
#[ignore = "requires explicit live-provider opt-in, rootless Podman and Linux user namespaces"]
#[cfg(feature = "fault-injection")]
async fn real_antigravity_correlated_implementer_fixture() -> Result<()> {
    run_antigravity_role_fixture(true, "implementer").await
}

#[cfg(feature = "fault-injection")]
async fn run_antigravity_role_fixture(correlated_candidate: bool, role_id: &str) -> Result<()> {
    let credential_catalog_pool = explicitly_authorized_live_credential_catalog().await?;
    let ctx = match setup_test().await {
        Ok(ctx) => ctx,
        Err(error) => {
            credential_catalog_pool.close().await;
            return Err(error);
        }
    };
    let mut audit_role_execution_id = None;
    let fixture_result = async {
        ensure_live_catalog_is_separate(
            &credential_catalog_pool,
            &ctx.engine.pool,
            &ctx.database.schema,
        )
        .await?;
        ensure_disposable_schema_has_no_credentials(&ctx.engine.pool).await?;

        let repo = tempdir()?;
        let p = repo.path();
        init_git_repo(p)?;

        fs::write(p.join("src.rs"), "fn original() {}\n")?;
        fs::write(p.join("README.md"), "# Synthetic arithmetic fixture\n")?;
        fs::write(p.join("test.sh"), "#!/bin/sh\nset -eu\nrustc --test src.rs -o /tmp/arithmetic-tests\n/tmp/arithmetic-tests\n")?;
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(p)
            .output()?;
        std::process::Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(p)
            .output()?;

        if role_id == "reviewer" {
            fs::write(p.join("src.rs"), "fn original() {}\npub fn multiply(a: i32, b: i32) -> i32 {\n    a * b\n}\n")?;
        }

        let candidate_repository_path = p.canonicalize()?.to_string_lossy().into_owned();
        let attempt_id = format!("att-{}", id());
        let created_workflow = ctx
            .store
            .create_workflow_run_full(
                "software_change_v1",
                &attempt_id,
                3,
                None,
                None,
                None,
                Some("Review addition of multiply function"),
                Some(&candidate_repository_path),
                None,
            )
            .await?;
        let wf = ctx
            .store
            .get_workflow_run(&created_workflow.id)
            .await?
            .context("Antigravity fixture workflow was not persisted")?;
        ensure!(
            wf.repository_path.as_deref() == Some(candidate_repository_path.as_str()),
            "Antigravity fixture workflow is not bound to its temporary repository"
        );

        let role = match role_id {
            "planner" => RoleDefinition::planner_v1(),
            "implementer" => RoleDefinition::implementer_v1(),
            "reviewer" => RoleDefinition::reviewer_v1(),
            _ => anyhow::bail!("unsupported fixture role"),
        };
        if role_id == "implementer" {
            ctx.store.pin_execution_profile(&wf.id, &orbit::execution::local::RoleExecutionProfile::DevLocal { bubblewrap: PathBuf::from("/usr/bin/bwrap") }).await?;
        }
        let mut selection_role = role.clone();
        if correlated_candidate {
            // Credential/quota selection uses the established runtime catalog.
            // Only this explicitly opted-in disposable fixture admits the
            // candidate image to collect evidence before capability promotion.
            selection_role.allowed_capabilities.required_tool_audit_correlation =
                Some(orbit::acp_capabilities::ToolAuditCorrelationCapability::Partial);
        }
        let mut target = resolve_live_target(&credential_catalog_pool, &selection_role, "antigravity", None).await?;
        if correlated_candidate {
            target.runtime_interface = "antigravity-correlated-candidate".into();
            target.runtime_image_digest = Some(orbit::acp_capabilities::ANTIGRAVITY_CORRELATED_IMAGE.into());
            target.resolution_reason.push_str("; qualification_candidate=UNQUALIFIED");
            // Exact qualification intent from the preceding session discovery.
            // This does not promote the normal runtime catalog.
            target.requested_model = Some("gemini-3.7-flash-high".into());
            target.resolved_model = Some("gemini-3.7-flash-high".into());
        }
        println!(
            "B3.4 live role selection: {}",
            serde_json::to_string(&sanitized_live_selection_summary(&target))?
        );
        let role_exec = ctx
            .store
            .create_role_execution(&wf.id, &role, &role_id.to_ascii_uppercase(), 0, None, None)
            .await?;
        audit_role_execution_id = Some(role_exec.id.clone());
        ctx.store.set_role_execution_resolved(&role_exec.id, &target).await?;
        let role_exec = ctx.store.get_role_execution(&role_exec.id).await?.context("fixture role missing")?;
        if role_id == "implementer" {
            ctx.store.acquire_workspace_mutation_lock(&attempt_id, &role_exec.id).await?;
        }

        let task = match role_id {
            "planner" => "Read src.rs using the advertised read callback and plan addition of a multiply function. Produce the plan handoff.",
            "implementer" => "Read src.rs using the advertised read callback. Add public multiply(a:i32,b:i32)->i32 using a*b, add a unit test asserting multiply(3,4)==12, and document multiply in README.md using the advertised mutation callbacks. Run exactly one orbit_terminal command `test ! -e /home/hieulc/.orbit/private && printf sandbox-ready` to check the confined terminal. Do not modify test.sh. Produce the implementation handoff.",
            _ => "Review the newly added multiply function in src.rs. Read src.rs using the advertised read callback before producing the review handoff.",
        };

        let outcome = RealAcpRoleExecutor
            .execute_role_with_credential_catalog(
                &ctx.engine.pool,
                &credential_catalog_pool,
                &wf,
                &role_exec,
                &role,
                &target,
                task,
                p,
                None,
                tokio::sync::watch::channel(false).1,
            )
            .await
            .map_err(|error| anyhow::anyhow!("live provider fixture execution failed: {error}"))?;

        ensure!(
            outcome.raw_output.contains("ORBIT_HANDOFF_START"),
            "reviewer did not return a structured handoff"
        );
        ensure!(
            outcome.termination_reason.as_deref() == Some("completed"),
            "reviewer fixture did not complete"
        );

        let lock_held = ctx
            .store
            .check_workspace_mutation_lock(&attempt_id, &role_exec.id)
            .await?;
        ensure!(lock_held == (role_id == "implementer"), "fixture mutation ownership differs from role authority");

        let payload: serde_json::Value = match role_id {
            "planner" => serde_json::to_value(extract_structured_envelope::<PlanHandoff>(&outcome.raw_output, "plan")?)?,
            "implementer" => serde_json::to_value(extract_structured_envelope::<ImplementationHandoff>(&outcome.raw_output, "implementation")?)?,
            _ => serde_json::to_value(extract_structured_envelope::<ReviewDecision>(&outcome.raw_output, "review")?)?,
        };
        let workspace = compute_workspace_state(p, "HEAD").await?;
        let handoff_type = match role_id { "planner" => HandoffType::Plan, "implementer" => HandoffType::Implementation, _ => HandoffType::Review };
        let handoff = ctx.store.save_handoff_artifact(&wf.id, Some(&role_exec.id), handoff_type, Some(&workspace.state_id), payload).await?;
        ensure!(ctx.store.get_handoff_artifact(&handoff.id).await?.context("handoff missing")?.workspace_state_id.as_deref() == Some(&workspace.state_id), "handoff lost candidate binding");

        if correlated_candidate {
            let execution_id = outcome.agent_execution_ids.first().context("missing agent execution")?;
            let (status, calls, successes, failures, counts, metadata) = load_agent_tool_audit(&ctx.engine.pool, execution_id).await?;
            ensure!(status == "SUCCEEDED" && calls > 0 && successes == calls && failures == 0, "candidate has unsuccessful or absent tool evidence");
            ensure!(b34_audit_has_exact_correlations(&metadata["tool_call_audit"], usize::try_from(calls)?), "candidate correlation evidence is incomplete or ambiguous");
            ensure!(metadata["observed_model"].as_str() == target.resolved_model.as_deref(), "selected model was not reported by runtime");
            ensure!(metadata["cleanup_confirmed"] == true, "candidate cleanup was not confirmed");
            if role_id == "implementer" {
                ensure!(counts["terminal.create"] == 1, "candidate did not exercise one atomic terminal callback");
                ensure!(counts["fs.write_text_file"].as_i64().unwrap_or(0) >= 2, "candidate did not exercise file mutation callbacks");
                ensure!(fs::read_to_string(p.join("src.rs"))?.contains("pub fn multiply"), "candidate source missing");
                use orbit::verification::{EnvironmentIdentity, VerificationPlan, VerificationStep, VerificationStore, VerificationRunResult, execute_verification_plan};
                let verification = execute_verification_plan(&VerificationStore::new(ctx.engine.pool.clone()), &attempt_id, &workspace,
                    &VerificationPlan::new("arithmetic", "Arithmetic candidate", vec![VerificationStep::new_command("arithmetic", "Rust arithmetic tests", vec!["sh".into(), "test.sh".into()])]), p,
                    EnvironmentIdentity { execution_profile:"sandboxed-container".into(), isolation:"rootless-podman".into(), oci_runtime:Some("podman".into()), runtime_image:Some("localhost/orbit-s9-verification@sha256:73fa989caa01cba6c284e53005aefd3f40a6be85f0f6a1612a7360b8e6342692".into()), runtime_image_digest:Some("sha256:8326e0c4dcd2dec8101272e317856a4608d8fb4c42ced2f8ab71e3068b96b047".into()), ..Default::default() }, None).await?;
                ensure!(verification.overall_result == Some(VerificationRunResult::Passed) && verification.workspace_state_id == workspace.state_id, "candidate authoritative verification failed or lost state binding: result={:?}, steps={:?}", verification.overall_result, verification.step_runs);
                ctx.store.release_workspace_mutation_lock(&attempt_id, &role_exec.id).await?;
            }
        }

        ensure_disposable_schema_has_no_credentials(&ctx.engine.pool).await?;
        Ok(())
    }
    .await;

    print_live_fixture_audit(&ctx.engine.pool, audit_role_execution_id.as_deref()).await;
    finish_live_fixture(credential_catalog_pool, ctx, fixture_result).await
}

#[tokio::test]
#[ignore = "requires explicit live-provider opt-in, guarded quota, rootless Podman and namespaces"]
#[cfg(feature = "fault-injection")]
async fn real_mixed_provider_workflow_and_operational_fallback() -> Result<()> {
    use orbit::{
        regression_strategy::{SelectionPolicy, VerificationCheck, VerificationTier},
        verification::{EnvironmentIdentity, VerificationPolicy, VerificationRunResult},
    };
    let catalog = explicitly_authorized_live_credential_catalog().await?;
    // Refuse before creating a workflow if there is no fresh, scoped headroom.
    if let Err(error) = guard_live_codex_quota(&catalog).await {
        catalog.close().await;
        return Err(error);
    }
    let ctx = match setup_test().await {
        Ok(ctx) => ctx,
        Err(error) => {
            catalog.close().await;
            return Err(error);
        }
    };
    let result = async {
        ensure_live_catalog_is_separate(&catalog, &ctx.engine.pool, &ctx.database.schema).await?;
        ensure_disposable_schema_has_no_credentials(&ctx.engine.pool).await?;
        let repo = common::TemporaryGitRepo::create()?;
        fs::write(repo.path().join("src.rs"), "fn original() {}\n")?;
        fs::write(repo.path().join("test.sh"), "#!/bin/sh\nset -eu\nrustc --test src.rs -o /tmp/arithmetic-tests\n/tmp/arithmetic-tests\n")?;
        let git = std::process::Command::new("git").args(["add", "."]).current_dir(repo.path()).output()?;
        ensure!(git.status.success(), "synthetic fixture staging failed");
        let git = std::process::Command::new("git").args(["commit", "-m", "synthetic arithmetic baseline"]).current_dir(repo.path()).output()?;
        ensure!(git.status.success(), "synthetic fixture commit failed");
        let mut policy = VerificationPolicy::new("arithmetic", "Arithmetic candidate");
        policy.required_steps = vec!["arithmetic".into()];
        let mut selection = SelectionPolicy::new("arithmetic", "Arithmetic candidate");
        selection.canonical_digest = true;
        selection.checks.push(VerificationCheck::new_command("arithmetic", "Arithmetic Rust tests", vec![VerificationTier::Fast, VerificationTier::Standard, VerificationTier::Full], vec!["sh".into(), "test.sh".into()]));
        ctx.store.verification_store().save_policy(&policy).await?;
        orbit::regression_strategy::RegressionStore::new(ctx.engine.pool.clone()).insert_selection_policy(&selection).await?;
        let task = "Inspect src.rs, add public multiply(a:i32,b:i32)->i32 using a*b and a unit test multiply(3,4)==12, and document multiply in README.md. Keep test.sh unchanged. Use advertised read and mutation callbacks. Each role must read src.rs. Produce structured role handoffs. This is a synthetic arithmetic qualification repository.";
        let workflow = ctx.store.create_workflow_run_full("arithmetic", &format!("attempt-{}", id()), 1, Some(&policy), None, Some(&selection), Some(task), repo.path().to_str(), Some("HEAD")).await?;
        let executor = std::sync::Arc::new(LiveCatalogRoleExecutor {catalog:catalog.clone(),injected:std::sync::Mutex::new(Default::default())});
        let coordinator = WorkflowCoordinator::new(ctx.engine.pool.clone(), executor).with_credential_catalog(catalog.clone()).with_verification_environment(EnvironmentIdentity {
            execution_profile:"sandboxed-container".into(),isolation:"rootless-podman".into(),oci_runtime:Some("podman".into()),runtime_image:Some("localhost/orbit-s9-verification@sha256:73fa989caa01cba6c284e53005aefd3f40a6be85f0f6a1612a7360b8e6342692".into()),runtime_image_digest:Some("sha256:8326e0c4dcd2dec8101272e317856a4608d8fb4c42ced2f8ab71e3068b96b047".into()),..Default::default()
        })?;
        for _ in 0..12 {
            let step = coordinator.step(&workflow.id).await?;
            if matches!(step, WorkflowStepResult::Terminal(_)) { break; }
        }
        let workflow = ctx.store.get_workflow_run(&workflow.id).await?.context("workflow missing")?;
        let roles = ctx.store.list_role_executions(&workflow.id).await?;
        for role in &roles { print_live_fixture_audit(&ctx.engine.pool, Some(&role.id)).await; }
        ensure!(workflow.status == WorkflowStage::Completed, "mixed workflow did not complete: {:?}", workflow.status);
        let mut successful_providers = std::collections::BTreeSet::new();
        let mut directions = std::collections::BTreeSet::new();
        for role in roles {
            ensure!(role.status == RoleExecutionStatus::Succeeded, "mixed role failed");
            let mut previous = None;
            for execution_id in &role.agent_execution_ids {
                let (status,calls,successes,failures,_,metadata) = load_agent_tool_audit(&ctx.engine.pool, execution_id).await?;
                let provider: String = sqlx::query_scalar("SELECT provider FROM orbit_agent_executions WHERE id=$1").bind(execution_id).fetch_one(&ctx.engine.pool).await?;
                if status == "FAILED" {
                    ensure!(metadata["fault_injection"]["boundary"] == "before_runtime_preparation" && metadata["lifecycle"]["prompt_uncertainty"] == "NOT_DISPATCHED" && calls == 0, "injected operational failure evidence incomplete");
                    previous = Some((provider,execution_id));
                } else {
                    ensure!(calls > 0 && successes == calls && failures == 0 && b34_audit_has_exact_correlations(&metadata["tool_call_audit"], usize::try_from(calls)?) && metadata["cleanup_confirmed"] == true, "successful mixed role lacks exact evidence");
                    successful_providers.insert(provider.clone());
                    if let Some((failed_provider,failed_id)) = previous.take() {
                        ensure!(metadata["continuation_from_agent_execution_id"].as_str() == Some(failed_id.as_str()), "continuation predecessor was lost");
                        directions.insert((failed_provider,provider));
                    }
                }
            }
        }
        ensure!(successful_providers.len() == 2 && directions.contains(&("codex".into(),"antigravity".into())) && directions.contains(&("antigravity".into(),"codex".into())), "mixed providers or fallback direction missing");
        let state = compute_workspace_state(repo.path(), "HEAD").await?;
        ensure!(workflow.current_workspace_state_id.as_deref() == Some(&state.state_id), "completed candidate lost workspace identity");
        let runs = ctx.store.verification_store().list_runs(&workflow.attempt_id).await?;
        ensure!(!runs.is_empty() && runs.iter().all(|run| run.workspace_state_id == state.state_id && run.overall_result == Some(VerificationRunResult::Passed)), "independent verification lost exact candidate binding");
        ensure_disposable_schema_has_no_credentials(&ctx.engine.pool).await?;
        Ok(())
    }.await;
    finish_live_fixture(catalog, ctx, result).await
}
