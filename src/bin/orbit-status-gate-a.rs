//! Operator-only, local and disposable Gate-A status qualification. This is
//! not an API-server endpoint, workflow Task, or provider inference adapter.
use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use orbit::{
    acp_runtime::Runtime,
    availability::{AvailabilityState, AvailabilityStore, ExecutionResourceIdentity, effective_at},
    codex_status_probe::{ProbeBinding, private_control_tempdir, probe_once},
    provider_scope::{BindingState, BindingStore, ObservationMode},
};
use serde::Deserialize;
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::{path::PathBuf, str::FromStr, time::Duration};
use tokio::io::AsyncReadExt;

#[derive(Parser)]
struct Cli {
    #[arg(long, env = "ORBIT_STATUS_PROBE_CONFIG")]
    config: PathBuf,
    #[arg(long, env = "ORBIT_STATUS_SELECTED_CREDENTIAL")]
    selected_credential: String,
    #[arg(long, env = "ORBIT_TEST_DATABASE_URL", hide_env_values = true)]
    database_url: String,
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// One authenticated observation, always UNKNOWN until separately confirmed.
    Observe,
    /// Read bounded binding state and append-only event history; no auth access.
    Inspect,
    /// Explicitly approve the observed fingerprint; no auth access.
    Confirm {
        #[arg(long)]
        fingerprint: String,
        #[arg(long, env = "ORBIT_STATUS_OPERATOR")]
        operator: String,
    },
    /// Explicitly reset a mismatched binding to unconfirmed; no auth access.
    Reenroll {
        #[arg(long)]
        fingerprint: String,
        #[arg(long, env = "ORBIT_STATUS_OPERATOR")]
        operator: String,
    },
    /// One final authenticated status probe; requires expected ID or confirmed binding.
    Probe,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QualificationConfig {
    runtime: Runtime,
    resource: ExecutionResourceIdentity,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let options = PgConnectOptions::from_str(&cli.database_url)?;
    ensure!(
        matches!(options.get_host(), "127.0.0.1" | "localhost"),
        "Gate-A database must be loopback-only"
    );
    ensure!(
        options
            .get_database()
            .is_some_and(|name| name.starts_with("orbit_status_")
                && name.len() > "orbit_status_".len()
                && name.len() <= 63),
        "Gate-A database must be a dedicated orbit_status_* database"
    );
    let mut bytes = Vec::new();
    tokio::fs::File::open(&cli.config)
        .await?
        .take(65_537)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(bytes.len() <= 65_536, "status config exceeds 64 KiB");
    let config: QualificationConfig = serde_json::from_slice(&bytes)?;
    config.runtime.validate()?;
    config.resource.validate()?;
    ensure!(
        config.runtime.auth.owner == cli.selected_credential
            && config.resource.credential.reference == cli.selected_credential
            && config.resource.credential.provider == config.runtime.auth.source,
        "selected logical credential does not match private config"
    );
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await?;
    let mut migration = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended(current_database() || ':orbit:status:migrations',0))")
        .execute(&mut *migration).await?;
    sqlx::raw_sql(include_str!("../../migrations/0007_availability.sql"))
        .execute(&mut *migration)
        .await?;
    sqlx::raw_sql(include_str!(
        "../../migrations/0008_provider_scope_bindings.sql"
    ))
    .execute(&mut *migration)
    .await?;
    migration.commit().await?;
    let store = BindingStore::new(&pool);
    match cli.action {
        Action::Inspect => {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "binding": store.inspect(&config.resource.credential).await?,
                    "history": store.history(&config.resource.credential).await?
                }))?
            );
        }
        Action::Confirm {
            fingerprint,
            operator,
        } => {
            let binding = store
                .confirm(&config.resource.credential, &fingerprint, &operator)
                .await?;
            println!("{}", serde_json::to_string_pretty(&binding)?);
        }
        Action::Reenroll {
            fingerprint,
            operator,
        } => {
            let binding = store
                .re_enroll(&config.resource.credential, &fingerprint, &operator)
                .await?;
            println!("{}", serde_json::to_string_pretty(&binding)?);
        }
        Action::Observe => {
            ensure!(
                std::env::var_os("ORBIT_STATUS_EXPECTED_ACCOUNT_ID").is_none(),
                "expected-ID mode cannot enroll"
            );
            let control = private_control_tempdir()?;
            let outcome = probe_once(
                &config.runtime,
                &config.resource,
                ProbeBinding::Enroll,
                control.path(),
                Duration::from_secs(60),
            )
            .await?;
            ensure!(
                outcome.receipt.cleanup_confirmed,
                "status cleanup unconfirmed"
            );
            ensure!(
                outcome.receipt.authenticated_account_present,
                "authenticated account not established"
            );
            let recorded = store
                .record_observation(
                    &config.resource.credential,
                    outcome.provider_scope_fingerprint.as_deref(),
                    ObservationMode::Enrollment,
                    outcome.snapshot,
                )
                .await?;
            ensure!(
                recorded.snapshot.state == AvailabilityState::Unknown,
                "enrollment established availability"
            );
            let enrolled = matches!(recorded.binding, Some(ref view) if view.state == BindingState::Unconfirmed);
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "binding": recorded.binding,
                    "snapshot_id": recorded.snapshot_id,
                    "availability": recorded.snapshot.state,
                    "enrollment_observed": enrolled,
                    "receipt": outcome.receipt
                }))?
            );
            ensure!(
                enrolled,
                "unconfirmed provider scope not established; inspect binding state"
            );
        }
        Action::Probe => {
            let expected = std::env::var("ORBIT_STATUS_EXPECTED_ACCOUNT_ID").ok();
            let binding = if let Some(ref expected) = expected {
                ProbeBinding::ExpectedAccountId(expected)
            } else {
                let confirmed = store
                    .inspect(&config.resource.credential)
                    .await?
                    .context("confirm a provider-scope binding before final probe")?;
                ensure!(
                    confirmed.state == BindingState::Confirmed,
                    "provider-scope binding is not confirmed"
                );
                // Keep the fingerprint alive across the external I/O.
                let (observed, qualified) =
                    probe_confirmed(&config, &store, &pool, &confirmed.fingerprint).await?;
                println!("{}", serde_json::to_string_pretty(&observed)?);
                ensure!(
                    qualified,
                    "confirmed provider scope not established; inspect binding state"
                );
                return Ok(());
            };
            let control = private_control_tempdir()?;
            let outcome = probe_once(
                &config.runtime,
                &config.resource,
                binding,
                control.path(),
                Duration::from_secs(60),
            )
            .await?;
            ensure!(
                outcome.receipt.cleanup_confirmed,
                "status cleanup unconfirmed"
            );
            ensure!(
                outcome.receipt.authenticated_account_present,
                "authenticated account not established"
            );
            let snapshot_id = AvailabilityStore::new(&pool)
                .record(&outcome.snapshot)
                .await?;
            let current = AvailabilityStore::new(&pool)
                .current_for(&config.resource)
                .await?;
            ensure!(
                current.contains(&outcome.snapshot),
                "snapshot current pointer mismatch"
            );
            let fresh = effective_at(&config.resource, &current, outcome.snapshot.observed_at_ms)?;
            let stale = effective_at(
                &config.resource,
                &current,
                outcome.snapshot.expires_at_ms + 1,
            )?;
            let qualified = outcome.receipt.account_scope_matched
                && outcome.snapshot.state != AvailabilityState::Unknown;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "mode":"expected_id", "snapshot_id":snapshot_id,
                    "snapshot":outcome.snapshot, "receipt":outcome.receipt,
                    "current_pointer_matches":true, "fresh_effective_state":fresh.state,
                    "stale_effective_state":stale.state,
                    "billable_request":"unknown", "provider_quota_cost":"unknown",
                    "provider_rate_limited":"unknown",
                    "inference_request_observed":"unknown_outside_protocol",
                    "qualified":qualified
                }))?
            );
            ensure!(qualified, "expected provider scope not established");
        }
    }
    Ok(())
}

async fn probe_confirmed(
    config: &QualificationConfig,
    store: &BindingStore<'_>,
    pool: &sqlx::PgPool,
    fingerprint: &str,
) -> Result<(serde_json::Value, bool)> {
    let control = private_control_tempdir()?;
    let outcome = probe_once(
        &config.runtime,
        &config.resource,
        ProbeBinding::ConfirmedFingerprint(fingerprint),
        control.path(),
        Duration::from_secs(60),
    )
    .await?;
    ensure!(
        outcome.receipt.cleanup_confirmed,
        "status cleanup unconfirmed"
    );
    ensure!(
        outcome.receipt.authenticated_account_present,
        "authenticated account not established"
    );
    let recorded = store
        .record_observation(
            &config.resource.credential,
            outcome.provider_scope_fingerprint.as_deref(),
            ObservationMode::Confirmed,
            outcome.snapshot,
        )
        .await?;
    let current = AvailabilityStore::new(pool)
        .current_for(&config.resource)
        .await?;
    ensure!(
        current.contains(&recorded.snapshot),
        "snapshot current pointer mismatch"
    );
    let fresh = effective_at(&config.resource, &current, recorded.snapshot.observed_at_ms)?;
    let stale = effective_at(
        &config.resource,
        &current,
        recorded.snapshot.expires_at_ms + 1,
    )?;
    let qualified = outcome.receipt.account_scope_matched
        && recorded.snapshot.state != AvailabilityState::Unknown
        && matches!(recorded.binding, Some(ref view) if view.state == BindingState::Confirmed);
    let result = json!({
        "mode":"confirmed_enrollment", "binding":recorded.binding,
        "snapshot_id":recorded.snapshot_id, "snapshot":recorded.snapshot,
        "receipt":outcome.receipt, "current_pointer_matches":true,
        "fresh_effective_state":fresh.state, "stale_effective_state":stale.state,
        "billable_request":"unknown", "provider_quota_cost":"unknown",
        "provider_rate_limited":"unknown",
        "inference_request_observed":"unknown_outside_protocol",
        "qualified":qualified
    });
    Ok((result, qualified))
}
