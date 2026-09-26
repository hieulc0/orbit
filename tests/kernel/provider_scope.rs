use super::*;
use orbit::{
    availability::{
        AvailabilityScope, AvailabilitySnapshot, AvailabilityState, AvailabilityStore,
        CredentialIdentity, EvidenceConfidence, EvidenceSource, ExecutionResourceIdentity,
        QuotaBucket, QuotaBucketWindow, QuotaWindow, RuntimeIdentity,
    },
    provider_scope::{BindingState, BindingStore, ObservationMode, fingerprint},
};

fn resource(generation: &str) -> ExecutionResourceIdentity {
    ExecutionResourceIdentity {
        runtime: RuntimeIdentity {
            family: "acp".into(),
            adapter: "codex_bridge".into(),
            binding: "fixture".into(),
            image_digest: format!("sha256:{}", "a".repeat(64)),
            agent_revision: "0.156.0".into(),
            adapter_version: "1".into(),
        },
        credential: CredentialIdentity {
            provider: "openai".into(),
            reference: "personal-fixture".into(),
            generation: generation.into(),
            catalog_id: None,
        },
        model: "fixture-model".into(),
        reasoning_effort: None,
    }
}

fn snapshot(resource: &ExecutionResourceIdentity, observed: i64) -> AvailabilitySnapshot {
    AvailabilitySnapshot {
        applies_to: AvailabilityScope::Credential(resource.credential.clone()),
        observed_at_ms: observed,
        expires_at_ms: observed + 100,
        state: AvailabilityState::Ready,
        quota_windows: vec![QuotaWindow {
            label: "bucket.opaque.primary".into(),
            duration_minutes: Some(60),
            used_percent: Some(31.0),
            remaining_percent: None,
            resets_at_ms: None,
            exhausted: None,
        }],
        quota_buckets: vec![QuotaBucket {
            provider_bucket_fingerprint: format!("qb1:{}", "c".repeat(64)),
            provider_label: None,
            scope: None,
            windows: vec![QuotaBucketWindow {
                provider_window_id: "primary".into(),
                duration_minutes: Some(60),
                used_percent: Some(31.0),
                remaining_percent: None,
                remaining_fraction: None,
                resets_at_ms: None,
                provider_reset_time: None,
                exhausted: None,
            }],
        }],
        quota_groups: vec![],
        source: EvidenceSource::RuntimeNativeStatus,
        confidence: EvidenceConfidence::AuthoritativeNative,
        source_revision: "codex-app-server-0.156.0".into(),
        evidence_digest: format!("sha256:{}", "b".repeat(64)),
        provider_observed_at_ms: None,
        provider_status_observation: Some(orbit::availability::ProviderStatusObservation {
            scope_state: orbit::availability::ProviderScopeEvidenceState::Unconfirmed,
            identity_value_comparison:
                orbit::availability::ProviderIdentityValueComparison::ExactValueMatch,
            quota_promotion: orbit::availability::QuotaEvidencePromotion::ObservedUnconfirmed,
            ordinary_usage_allowed: Some(true),
        }),
    }
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; set ORBIT_TEST_DATABASE_URL"]
async fn provider_scope_enrollment_confirmation_mismatch_and_generation_are_durable() -> Result<()>
{
    let f = Fixture::new().await?;
    let r = resource("1");
    let store = BindingStore::new(&f.engine.pool);
    let first = fingerprint("openai", "personal-a").unwrap();
    let other = fingerprint("openai", "personal-b").unwrap();
    let observed = store
        .record_observation(
            &r.credential,
            Some(&first),
            ObservationMode::Enrollment,
            snapshot(&r, 10),
        )
        .await?;
    assert_eq!(
        observed.binding.as_ref().unwrap().state,
        BindingState::Unconfirmed
    );
    assert_eq!(observed.snapshot.state, AvailabilityState::Unknown);
    assert_eq!(observed.snapshot.quota_windows.len(), 1);
    assert_eq!(observed.snapshot.quota_buckets.len(), 1);
    assert_eq!(
        observed
            .snapshot
            .provider_status_observation
            .as_ref()
            .unwrap()
            .quota_promotion,
        orbit::availability::QuotaEvidencePromotion::ObservedUnconfirmed
    );
    assert_eq!(
        AvailabilityStore::new(&f.engine.pool)
            .current_for(&r)
            .await?,
        vec![observed.snapshot.clone()]
    );
    assert!(
        store
            .confirm(&r.credential, &other, "operator")
            .await
            .is_err()
    );
    assert_eq!(
        store
            .confirm(&r.credential, &first, "operator")
            .await?
            .state,
        BindingState::Confirmed
    );
    assert_eq!(
        store
            .confirm(&r.credential, &first, "operator")
            .await?
            .state,
        BindingState::Confirmed
    );
    let history = store.history(&r.credential).await?;
    assert_eq!(history.len(), 2);
    assert_eq!(
        history
            .iter()
            .find(|event| event.event_kind == "observed")
            .and_then(|event| event.snapshot_id.as_deref()),
        Some(observed.snapshot_id.as_str())
    );
    assert!(
        sqlx::query("UPDATE orbit_provider_scope_binding_events SET actor='tampered'")
            .execute(&f.engine.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM orbit_provider_scope_binding_events")
            .execute(&f.engine.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("TRUNCATE orbit_provider_scope_binding_events")
            .execute(&f.engine.pool)
            .await
            .is_err()
    );
    assert_eq!(store.history(&r.credential).await?.len(), 2);
    // Confirmation cannot retroactively promote the first snapshot.
    assert_eq!(
        AvailabilityStore::new(&f.engine.pool)
            .current_for(&r)
            .await?,
        vec![observed.snapshot]
    );
    let later = store
        .record_observation(
            &r.credential,
            Some(&first),
            ObservationMode::Confirmed,
            snapshot(&r, 20),
        )
        .await?;
    assert_eq!(later.snapshot.state, AvailabilityState::Ready);
    assert_eq!(later.snapshot.quota_buckets.len(), 1);
    assert_eq!(
        later
            .snapshot
            .provider_status_observation
            .as_ref()
            .unwrap()
            .quota_promotion,
        orbit::availability::QuotaEvidencePromotion::Promoted
    );
    let mismatch = store
        .record_observation(
            &r.credential,
            Some(&other),
            ObservationMode::Confirmed,
            snapshot(&r, 20),
        )
        .await?;
    assert_eq!(mismatch.snapshot.state, AvailabilityState::Unknown);
    assert_eq!(mismatch.snapshot.quota_windows.len(), 1);
    assert_eq!(mismatch.snapshot.quota_buckets.len(), 1);
    assert!(mismatch.snapshot.observed_at_ms > later.snapshot.observed_at_ms);
    assert_eq!(
        mismatch.binding.as_ref().unwrap().state,
        BindingState::Mismatch
    );
    assert_eq!(mismatch.binding.as_ref().unwrap().fingerprint, first);
    assert_eq!(
        mismatch
            .binding
            .as_ref()
            .unwrap()
            .mismatch_fingerprint
            .as_deref(),
        Some(other.as_str())
    );
    assert!(
        store
            .confirm(&r.credential, &other, "operator")
            .await
            .is_err()
    );
    assert_eq!(
        AvailabilityStore::new(&f.engine.pool)
            .current_for(&r)
            .await?,
        vec![mismatch.snapshot]
    );
    let restarted = Engine::connect(&f.url, f.engine.artifact_root.clone(), 3).await?;
    assert_eq!(
        BindingStore::new(&restarted.pool)
            .inspect(&r.credential)
            .await?
            .unwrap()
            .state,
        BindingState::Mismatch
    );
    let fresh_generation = resource("2");
    assert!(store.inspect(&fresh_generation.credential).await?.is_none());
    let changed = store
        .record_observation(
            &fresh_generation.credential,
            Some(&first),
            ObservationMode::Confirmed,
            snapshot(&fresh_generation, 40),
        )
        .await?;
    assert_eq!(changed.snapshot.state, AvailabilityState::Unknown);
    assert!(changed.binding.is_none());
    let changed = store
        .record_observation(
            &fresh_generation.credential,
            Some(&first),
            ObservationMode::Enrollment,
            snapshot(&fresh_generation, 41),
        )
        .await?;
    assert_eq!(changed.binding.unwrap().state, BindingState::Unconfirmed);
    let reenrolled = store.re_enroll(&r.credential, &other, "operator").await?;
    assert_eq!(reenrolled.state, BindingState::Unconfirmed);
    assert_eq!(
        store
            .confirm(&r.credential, &other, "operator")
            .await?
            .state,
        BindingState::Confirmed
    );
    assert_eq!(store.history(&r.credential).await?.len(), 7);
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; set ORBIT_TEST_DATABASE_URL"]
async fn provider_scope_confirmation_rejects_inconsistent_identity_values() -> Result<()> {
    let fixture = Fixture::new().await?;
    let resource = resource("1");
    let store = BindingStore::new(&fixture.engine.pool);
    let observed_scope = fingerprint("openai", "synthetic-codex-scope").unwrap();
    let mut contradictory = snapshot(&resource, 10);
    let evidence = contradictory
        .provider_status_observation
        .as_mut()
        .expect("synthetic status metadata");
    evidence.identity_value_comparison =
        orbit::availability::ProviderIdentityValueComparison::ExactValueMismatch;
    evidence.quota_promotion =
        orbit::availability::QuotaEvidencePromotion::WithheldIdentityMismatch;

    let recorded = store
        .record_observation(
            &resource.credential,
            Some(&observed_scope),
            ObservationMode::Enrollment,
            contradictory,
        )
        .await?;
    assert_eq!(recorded.snapshot.state, AvailabilityState::Unknown);
    assert_eq!(recorded.snapshot.quota_buckets.len(), 1);
    assert_eq!(
        recorded
            .snapshot
            .provider_status_observation
            .as_ref()
            .unwrap()
            .quota_promotion,
        orbit::availability::QuotaEvidencePromotion::WithheldIdentityMismatch
    );
    assert!(
        store
            .confirm(&resource.credential, &observed_scope, "operator")
            .await
            .is_err()
    );
    assert_eq!(
        store.inspect(&resource.credential).await?.unwrap().state,
        BindingState::Unconfirmed
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; set ORBIT_TEST_DATABASE_URL"]
async fn unconfirmed_codex_quota_is_durable_without_raw_provider_ids() -> Result<()> {
    let fixture = Fixture::new().await?;
    let resource = resource("1");
    let account_id = "synthetic-codex-account-id";
    let limit_id = "synthetic-codex-limit-id";
    let result = serde_json::json!({
        "accountId": account_id,
        "ordinaryUsageAllowed": true,
        "rateLimits": {"primary": null, "secondary": null},
        "rateLimitsByLimitId": {
            (limit_id): {
                "limitId": limit_id,
                "limitName": "synthetic-codex-plan",
                "primary": {
                    "usedPercent": 31,
                    "windowDurationMins": 300,
                    "resetsAt": 1_730_947_200_i64
                },
                "secondary": {
                    "usedPercent": 46,
                    "windowDurationMins": 10_080,
                    "resetsAt": 1_731_542_400_i64
                }
            }
        }
    });
    let mut observation = orbit::provider_status::codex_rate_limits_snapshot(
        &resource,
        "",
        &serde_json::to_vec(&result)?,
        10,
        110,
    )?;
    observation
        .provider_status_observation
        .as_mut()
        .expect("Codex status metadata")
        .identity_value_comparison =
        orbit::availability::ProviderIdentityValueComparison::ExactValueMatch;
    let scope_fingerprint = fingerprint(&resource.credential.provider, account_id).unwrap();
    let recorded = BindingStore::new(&fixture.engine.pool)
        .record_observation(
            &resource.credential,
            Some(&scope_fingerprint),
            ObservationMode::Enrollment,
            observation,
        )
        .await?;

    assert_eq!(recorded.snapshot.state, AvailabilityState::Unknown);
    assert_eq!(recorded.snapshot.quota_buckets.len(), 1);
    assert_eq!(recorded.snapshot.quota_buckets[0].windows.len(), 2);
    assert_eq!(
        recorded
            .snapshot
            .provider_status_observation
            .as_ref()
            .unwrap()
            .quota_promotion,
        orbit::availability::QuotaEvidencePromotion::ObservedUnconfirmed
    );

    let evidence: serde_json::Value =
        sqlx::query_scalar("SELECT evidence FROM orbit_availability_snapshots WHERE id=$1")
            .bind(&recorded.snapshot_id)
            .fetch_one(&fixture.engine.pool)
            .await?;
    let binding: serde_json::Value = sqlx::query_scalar(
        "SELECT to_jsonb(b) FROM orbit_provider_scope_bindings b WHERE credential->>'reference'=$1",
    )
    .bind(&resource.credential.reference)
    .fetch_one(&fixture.engine.pool)
    .await?;
    let events: Vec<serde_json::Value> = sqlx::query_scalar(
        "SELECT to_jsonb(e) FROM orbit_provider_scope_binding_events e JOIN orbit_provider_scope_bindings b USING (credential_key) WHERE b.credential->>'reference'=$1",
    )
    .bind(&resource.credential.reference)
    .fetch_all(&fixture.engine.pool)
    .await?;
    let durable = serde_json::to_string(&(&evidence, &binding, &events))?;
    assert!(durable.contains("synthetic-codex-plan"));
    assert!(durable.contains("used_percent"));
    assert!(!durable.contains(account_id));
    assert!(!durable.contains(limit_id));
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; set ORBIT_TEST_DATABASE_URL"]
async fn provider_scope_concurrent_confirmation_is_single_auditable_transition() -> Result<()> {
    let f = Fixture::new().await?;
    let r = resource("1");
    let store = BindingStore::new(&f.engine.pool);
    let value = fingerprint("openai", "personal-a").unwrap();
    store
        .record_observation(
            &r.credential,
            Some(&value),
            ObservationMode::Enrollment,
            snapshot(&r, 10),
        )
        .await?;
    let (a, b) = tokio::join!(
        store.confirm(&r.credential, &value, "operator-a"),
        store.confirm(&r.credential, &value, "operator-b")
    );
    assert_eq!(a?.state, BindingState::Confirmed);
    assert_eq!(b?.state, BindingState::Confirmed);
    assert_eq!(store.history(&r.credential).await?.len(), 2);
    assert_eq!(
        store.inspect(&r.credential).await?.unwrap().state,
        BindingState::Confirmed
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; set ORBIT_TEST_DATABASE_URL"]
async fn invalid_or_failed_status_persistence_cannot_partially_promote_scope() -> Result<()> {
    let fixture = Fixture::new().await?;
    let resource = resource("1");
    let bindings = BindingStore::new(&fixture.engine.pool);
    let availability = AvailabilityStore::new(&fixture.engine.pool);
    let observed_scope = fingerprint("openai", "synthetic-provider-account").unwrap();

    let mut invalid = snapshot(&resource, 10);
    invalid.expires_at_ms = invalid.observed_at_ms;
    assert!(
        bindings
            .record_observation(
                &resource.credential,
                Some(&observed_scope),
                ObservationMode::Enrollment,
                invalid,
            )
            .await
            .is_err()
    );
    assert!(bindings.inspect(&resource.credential).await?.is_none());
    assert!(availability.current_for(&resource).await?.is_empty());

    let enrollment = bindings
        .record_observation(
            &resource.credential,
            Some(&observed_scope),
            ObservationMode::Enrollment,
            snapshot(&resource, 20),
        )
        .await?;
    assert_eq!(enrollment.snapshot.state, AvailabilityState::Unknown);
    bindings
        .confirm(&resource.credential, &observed_scope, "operator")
        .await?;
    let baseline = availability.current_for(&resource).await?;
    assert_eq!(baseline.len(), 1);
    assert_eq!(baseline[0].state, AvailabilityState::Unknown);
    let snapshot_count_before: i64 =
        sqlx::query_scalar("SELECT count(*) FROM orbit_availability_snapshots")
            .fetch_one(&fixture.engine.pool)
            .await?;
    let event_count_before: i64 =
        sqlx::query_scalar("SELECT count(*) FROM orbit_provider_scope_binding_events")
            .fetch_one(&fixture.engine.pool)
            .await?;

    // Fail the current-pointer write after status/scope rows were staged in
    // the same transaction. None of that attempted promotion may commit.
    sqlx::query("CREATE FUNCTION orbit_test_fail_status_pointer() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'synthetic persistence failure'; END $$")
        .execute(&fixture.engine.pool)
        .await?;
    sqlx::query("CREATE TRIGGER orbit_test_fail_status_pointer BEFORE INSERT OR UPDATE ON orbit_availability_current FOR EACH ROW EXECUTE FUNCTION orbit_test_fail_status_pointer()")
        .execute(&fixture.engine.pool)
        .await?;
    assert!(
        bindings
            .record_observation(
                &resource.credential,
                Some(&observed_scope),
                ObservationMode::Confirmed,
                snapshot(&resource, 30),
            )
            .await
            .is_err()
    );

    assert_eq!(
        bindings.inspect(&resource.credential).await?.unwrap().state,
        BindingState::Confirmed
    );
    assert_eq!(
        bindings.history(&resource.credential).await?.len() as i64,
        event_count_before
    );
    assert_eq!(availability.current_for(&resource).await?, baseline);
    let snapshot_count_after: i64 =
        sqlx::query_scalar("SELECT count(*) FROM orbit_availability_snapshots")
            .fetch_one(&fixture.engine.pool)
            .await?;
    assert_eq!(snapshot_count_after, snapshot_count_before);
    Ok(())
}
