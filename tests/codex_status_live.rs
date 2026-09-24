//! Explicitly selected, single-credential live qualification only. Ignored by
//! default; never creates a Task, Attempt, AgentExecution or repository.
use anyhow::{Context, Result, ensure};
use orbit::{
    acp_runtime::Runtime,
    availability::{AvailabilityState, AvailabilityStore, ExecutionResourceIdentity, effective_at},
    codex_status_probe::{ProbeBinding, private_control_tempdir, probe_once},
    engine::Engine,
};
use serde::Deserialize;
use serde_json::json;
use std::{path::Path, time::Duration};
use tokio::io::AsyncReadExt;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QualificationConfig {
    runtime: Runtime,
    resource: ExecutionResourceIdentity,
}

#[tokio::test]
#[ignore = "one authorized Codex 0.156.0 status probe; requires dedicated credential binding and disposable PostgreSQL"]
async fn one_pinned_codex_status_probe() -> Result<()> {
    let config_path = std::env::var("ORBIT_STATUS_PROBE_CONFIG")
        .context("select a dedicated private status-probe runtime/resource config")?;
    let expected_account_id = std::env::var("ORBIT_STATUS_EXPECTED_ACCOUNT_ID")
        .context("supply the expected account ID out of band; never print it")?;
    let selected_credential = std::env::var("ORBIT_STATUS_SELECTED_CREDENTIAL")
        .context("explicitly select the dedicated qualification credential by logical reference")?;
    let database_url = std::env::var("ORBIT_TEST_DATABASE_URL")
        .context("set ORBIT_TEST_DATABASE_URL to disposable PostgreSQL only")?;
    let mut bytes = Vec::new();
    tokio::fs::File::open(Path::new(&config_path))
        .await?
        .take(65_537)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(bytes.len() <= 65_536, "status config exceeds 64 KiB");
    let config: QualificationConfig = serde_json::from_slice(&bytes)?;
    ensure!(
        config.runtime.auth.owner == selected_credential
            && config.resource.credential.reference == selected_credential,
        "status probe credential selection does not match runtime/resource"
    );
    let admin = sqlx::PgPool::connect(&database_url).await?;
    let schema = format!("orbit_status_{}", orbit::model::id().replace('-', ""));
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await?;
    admin.close().await;
    let separator = if database_url.contains('?') { '&' } else { '?' };
    let isolated_url = format!("{database_url}{separator}options=-csearch_path%3D{schema}");
    let control = private_control_tempdir()?;
    let artifacts = tempfile::tempdir()?;
    let engine = Engine::connect(&isolated_url, artifacts.path().to_path_buf(), 3).await?;
    let outcome = match probe_once(
        &config.runtime,
        &config.resource,
        ProbeBinding::ExpectedAccountId(&expected_account_id),
        control.path(),
        Duration::from_secs(60),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(failure) => {
            eprintln!("{}", serde_json::to_string(&failure)?);
            return Err(failure.into());
        }
    };
    ensure!(
        outcome.receipt.authenticated_account_present && outcome.receipt.account_scope_matched,
        "returned account does not match selected credential"
    );
    ensure!(
        outcome.receipt.cleanup_confirmed,
        "credential cleanup unconfirmed"
    );
    ensure!(
        outcome.snapshot.state != AvailabilityState::Unknown,
        "status response is not qualifying availability evidence"
    );
    let store = AvailabilityStore::new(&engine.pool);
    let snapshot_id = store.record(&outcome.snapshot).await?;
    let current = store.current_for(&config.resource).await?;
    ensure!(
        current == vec![outcome.snapshot.clone()],
        "stored snapshot differs from normalized observation"
    );
    let fresh = effective_at(&config.resource, &current, outcome.snapshot.observed_at_ms)?;
    let stale = effective_at(
        &config.resource,
        &current,
        outcome.snapshot.expires_at_ms + 1,
    )?;
    ensure!(
        stale.state == AvailabilityState::Unknown,
        "stale positive evidence remained READY"
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "runtime_version":config.runtime.launch.binary_revision,
            "runtime_image":config.runtime.launch.image,
            "logical_resource_id":config.resource.id()?,
            "protocol_method":"account/rateLimits/read",
            "receipt":outcome.receipt,
            "snapshot_id":snapshot_id,
            "snapshot":outcome.snapshot,
            "current_pointer_matches":true,
            "fresh_effective_state":fresh.state,
            "stale_effective_state":stale.state,
            "inference_request_observed":"unknown_outside_protocol",
            "billable_request":"unknown",
            "provider_quota_cost":"unknown",
            "provider_rate_limited":"unknown"
        }))?
    );
    Ok(())
}
