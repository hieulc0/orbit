use super::*;
use orbit::availability::{
    AvailabilityScope, AvailabilitySnapshot, AvailabilityState, AvailabilityStore,
    CredentialIdentity, EvidenceConfidence, EvidenceSource, ExecutionResourceIdentity,
    RuntimeIdentity, effective_at,
};

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; set ORBIT_TEST_DATABASE_URL"]
async fn availability_history_is_idempotent_and_current_is_monotonic() -> Result<()> {
    let f = Fixture::new().await?;
    let resource = ExecutionResourceIdentity {
        runtime: RuntimeIdentity {
            family: "acp".into(),
            adapter: "codex_bridge".into(),
            binding: "fixture".into(),
            image_digest: format!("sha256:{}", "a".repeat(64)),
            agent_revision: "0.156.0".into(),
            adapter_version: "1".into(),
        },
        credential: CredentialIdentity {
            provider: "fixture".into(),
            reference: "account-1".into(),
            generation: "1".into(),
            catalog_id: None,
        },
        model: "model/one".into(),
        reasoning_effort: None,
    };
    let evidence = |state, observed_at_ms| AvailabilitySnapshot {
        applies_to: AvailabilityScope::Exact(resource.clone()),
        observed_at_ms,
        expires_at_ms: 100,
        state,
        quota_windows: vec![],
        quota_buckets: vec![],
        quota_groups: vec![],
        source: EvidenceSource::ExecutionResult,
        confidence: EvidenceConfidence::ExecutionObserved,
        source_revision: "fixture".into(),
        evidence_digest: format!("sha256:{}", "b".repeat(64)),
        provider_observed_at_ms: None,
    };
    let old = evidence(AvailabilityState::Ready, 10);
    let new = evidence(AvailabilityState::QuotaExhausted, 20);
    let store = AvailabilityStore::new(&f.engine.pool);
    store.record(&new).await?;
    store.record(&old).await?;
    store.record(&new).await?;
    let current = store.current_for(&resource).await?;
    assert_eq!(current, vec![new.clone()]);
    assert_eq!(
        effective_at(&resource, &current, 30)?.state,
        AvailabilityState::QuotaExhausted
    );
    let history_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM orbit_availability_snapshots")
            .fetch_one(&f.engine.pool)
            .await?;
    assert_eq!(history_count, 2);
    let restarted = Engine::connect(&f.url, f.engine.artifact_root.clone(), 3).await?;
    assert_eq!(
        AvailabilityStore::new(&restarted.pool)
            .current_for(&resource)
            .await?,
        current
    );
    let later = evidence(AvailabilityState::Ready, 30);
    let latest = evidence(AvailabilityState::RateLimited, 40);
    let (first, second) = tokio::join!(store.record(&latest), store.record(&later));
    first?;
    second?;
    assert_eq!(store.current_for(&resource).await?, vec![latest]);
    let status = orbit::provider_status::codex_rate_limits_snapshot(
        &resource,
        "fixture-account-id",
        br#"{"accountId":"fixture-account-id","ordinaryUsageAllowed":true,"rateLimits":{"primary":null,"secondary":null},"rateLimitsByLimitId":{"group-a":{"limitId":"group-a","primary":{"usedPercent":25,"windowDurationMins":15,"resetsAt":null},"secondary":{"usedPercent":46,"windowDurationMins":10080,"resetsAt":null}}}}"#,
        50,
        100,
    )?;
    store.record(&status).await?;
    let evidence = store.current_for(&resource).await?;
    let persisted_status = evidence
        .iter()
        .find(|snapshot| matches!(snapshot.applies_to, AvailabilityScope::Credential(_)))
        .expect("credential-scoped status snapshot");
    assert_eq!(persisted_status.quota_buckets.len(), 1);
    assert_eq!(persisted_status.quota_buckets[0].windows.len(), 2);
    assert_eq!(
        persisted_status.quota_buckets[0].windows[0].provider_window_id,
        "primary"
    );
    assert_eq!(
        persisted_status.quota_buckets[0].windows[1].provider_window_id,
        "secondary"
    );
    assert_eq!(evidence.len(), 2);
    assert_eq!(
        effective_at(&resource, &evidence, 60)?.state,
        AvailabilityState::RateLimited
    );
    assert_eq!(
        effective_at(&resource, &evidence, 101)?.state,
        AvailabilityState::Unknown
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; set ORBIT_TEST_DATABASE_URL"]
async fn malformed_or_unclassified_status_does_not_write_availability() -> Result<()> {
    let fixture = Fixture::new().await?;
    let resource = ExecutionResourceIdentity {
        runtime: RuntimeIdentity {
            family: "acp".into(),
            adapter: "fixture".into(),
            binding: "status-failure-fixture".into(),
            image_digest: format!("sha256:{}", "c".repeat(64)),
            agent_revision: "fixture-v1".into(),
            adapter_version: "1".into(),
        },
        credential: CredentialIdentity {
            provider: "antigravity".into(),
            reference: "synthetic-status-fixture".into(),
            generation: "1".into(),
            catalog_id: Some(orbit::model::id()),
        },
        model: "unknown-model".into(),
        reasoning_effort: None,
    };

    assert!(orbit::agy_usage_schema::parse_schema_only(b"{malformed status fixture}").is_err());
    let private_marker = "synthetic-private-status-value";
    let diagnostic = orbit::agy_usage_schema::parse_schema_only(
        format!(
            r#"{{"quota":{{"bucket":{{"primary":{{"remaining_fraction":0.5}}}}}},"account":{{"access_token":"{private_marker}"}},"future_identity":"{private_marker}"}}"#
        )
        .as_bytes(),
    )?;
    assert!(!diagnostic.raw_persistence_allowed_by_classification);
    let diagnostic_json = serde_json::to_string(&diagnostic)?;
    assert!(diagnostic_json.contains("$.quota.bucket.primary.remaining_fraction"));
    assert!(diagnostic_json.contains("exact-sensitive-key"));
    assert!(diagnostic_json.contains("not-explicitly-classified-safe"));
    assert!(!diagnostic_json.contains(private_marker));

    // Invalid synthetic evidence must be rejected before it can become a
    // durable availability snapshot.
    let invalid = AvailabilitySnapshot {
        applies_to: AvailabilityScope::Credential(resource.credential.clone()),
        observed_at_ms: 10,
        expires_at_ms: 10,
        state: AvailabilityState::Ready,
        quota_windows: vec![],
        quota_buckets: vec![],
        quota_groups: vec![],
        source: EvidenceSource::ProviderNativeStatus,
        confidence: EvidenceConfidence::AuthoritativeNative,
        source_revision: "synthetic".into(),
        evidence_digest: format!("sha256:{}", "d".repeat(64)),
        provider_observed_at_ms: None,
    };
    let store = AvailabilityStore::new(&fixture.engine.pool);
    assert!(store.record(&invalid).await.is_err());
    assert!(store.current_for(&resource).await?.is_empty());
    let persisted: i64 = sqlx::query_scalar("SELECT count(*) FROM orbit_availability_snapshots")
        .fetch_one(&fixture.engine.pool)
        .await?;
    assert_eq!(persisted, 0);
    let evidence_json: String = sqlx::query_scalar(
        "SELECT coalesce(string_agg(evidence::text, E'\\n'), '') FROM orbit_availability_snapshots",
    )
    .fetch_one(&fixture.engine.pool)
    .await?;
    assert!(!evidence_json.contains(private_marker));
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; set ORBIT_TEST_DATABASE_URL"]
async fn antigravity_synthetic_quota_capture_roundtrips_without_raw_values_and_rolls_back()
-> Result<()> {
    let fixture = Fixture::new().await?;
    let credential = CredentialIdentity {
        provider: "antigravity".into(),
        reference: "synthetic-status-fixture".into(),
        generation: "1".into(),
        catalog_id: Some("00000000-0000-4000-8000-000000000002".into()),
    };
    let resource = ExecutionResourceIdentity {
        runtime: RuntimeIdentity {
            family: "acp".into(),
            adapter: "antigravity_fixture".into(),
            binding: "synthetic-status-runtime".into(),
            image_digest: format!("sha256:{}", "e".repeat(64)),
            agent_revision: "fixture-v1".into(),
            adapter_version: "1".into(),
        },
        credential: credential.clone(),
        model: "unqualified-model".into(),
        reasoning_effort: None,
    };
    let document = serde_json::json!({
        "command":{"data":{"groups":[
            {"id":"pg-synthetic-group-a","name":"Synthetic Group A",
             "description":"Models within this group: Synthetic Model A",
             "buckets":[
                {"id":"pg-synthetic-bucket-a","name":"pg-private-label-a","window":"primary",
                 "remaining_fraction":0.375,"reset_time":"2026-03-04T05:06:07Z",
                 "future_field":"pg-private-unknown-value"},
                {"id":"pg-synthetic-bucket-a","name":"pg-private-label-a","window":"secondary",
                 "remaining_fraction":null,"reset_time":null}
            ]},
            {"id":"pg-synthetic-group-b","name":"Synthetic Group B",
             "description":"Models within this group: Synthetic Model B","buckets":[
                {"id":"pg-synthetic-bucket-b","window":"5h","remaining_fraction":null}
            ]}
        ]}},
        "account":{"access_token":"pg-private-token-value"},
        "usage":{"input_tokens":987654}
    });
    let raw = serde_json::to_vec(&document)?;
    let capture = orbit::provider_status::antigravity_usage_capture(&raw)?;
    assert!(capture.normalization_ready);
    assert!(capture.group_metadata_ready);
    assert!(capture.membership_ready);
    let snapshot = capture.availability_snapshot(credential.clone(), 1_000, 10_000)?;
    assert_eq!(snapshot.state, AvailabilityState::Unknown);
    assert_eq!(snapshot.quota_buckets.len(), 2);
    assert_eq!(snapshot.quota_groups.len(), 2);
    let store = AvailabilityStore::new(&fixture.engine.pool);
    let first_id = store.record(&snapshot).await?;
    assert_eq!(store.record(&snapshot).await?, first_id);
    let persisted = store.current_for(&resource).await?;
    assert_eq!(persisted, vec![snapshot.clone()]);
    assert_eq!(persisted[0].quota_groups, snapshot.quota_groups);
    let group_a = persisted[0]
        .quota_groups
        .iter()
        .find(|group| group.provider_display_name.as_deref() == Some("Synthetic Group A"))
        .unwrap();
    assert_eq!(group_a.members[0].provider_label, "Synthetic Model A");
    assert_eq!(group_a.bucket_fingerprints.len(), 1);
    assert_eq!(
        persisted[0]
            .quota_buckets
            .iter()
            .find(|bucket| bucket.provider_bucket_fingerprint == group_a.bucket_fingerprints[0])
            .unwrap()
            .windows
            .len(),
        2
    );
    assert_eq!(
        persisted[0]
            .quota_buckets
            .iter()
            .find(|bucket| bucket.windows.len() == 2)
            .unwrap()
            .windows
            .len(),
        2
    );
    assert!(
        persisted[0]
            .quota_buckets
            .iter()
            .flat_map(|bucket| bucket.windows.iter())
            .any(|window| window.provider_window_id == "secondary"
                && window.remaining_fraction.is_none()
                && window.provider_reset_time.is_none()
                && window.resets_at_ms.is_none())
    );
    assert!(
        persisted[0]
            .quota_buckets
            .iter()
            .flat_map(|bucket| bucket.windows.iter())
            .any(
                |window| window.provider_reset_time.as_deref() == Some("2026-03-04T05:06:07Z")
                    && window.resets_at_ms == Some(1_772_600_767_000)
            )
    );

    let evidence_json: String = sqlx::query_scalar(
        "SELECT coalesce(string_agg(evidence::text, E'\\n'), '') FROM orbit_availability_snapshots",
    )
    .fetch_one(&fixture.engine.pool)
    .await?;
    for raw_value in [
        "pg-synthetic-group-a",
        "pg-synthetic-group-b",
        "pg-synthetic-bucket-a",
        "pg-synthetic-bucket-b",
        "pg-private-label-a",
        "pg-private-unknown-value",
        "pg-private-token-value",
        "pg-synthetic-group-a",
        "pg-synthetic-group-b",
        "987654",
        "future_field",
        "access_token",
        "\"groups\"",
        "\"command\"",
    ] {
        assert!(
            !evidence_json.contains(raw_value),
            "persisted raw data: {raw_value}"
        );
    }
    assert!(evidence_json.len() < 16_384);
    assert!(evidence_json.contains("remaining_fraction"));
    assert!(evidence_json.contains("provider_reset_time"));
    assert!(evidence_json.contains("Synthetic Group A"));
    assert!(evidence_json.contains("Synthetic Model A"));
    assert!(evidence_json.contains("qg1:"));
    assert!(evidence_json.contains("qb1:"));
    assert_eq!(
        snapshot.quota_buckets[0].provider_bucket_fingerprint,
        capture
            .availability_snapshot(credential.clone(), 1_000, 10_000)?
            .quota_buckets[0]
            .provider_bucket_fingerprint
    );

    // A malformed finite/type check creates no snapshot object and therefore
    // cannot partially promote evidence in PostgreSQL.
    let malformed = serde_json::json!({"command":{"data":{"groups":[{"buckets":[
        {"id":"synthetic-invalid","window":"primary","remaining_fraction":"0.2"}
    ]}]}}});
    let rejected =
        orbit::provider_status::antigravity_usage_capture(&serde_json::to_vec(&malformed)?)?;
    assert!(!rejected.normalization_ready);
    assert!(
        rejected
            .availability_snapshot(credential.clone(), 2_000, 10_000)
            .is_err()
    );

    // Force the current-pointer write to fail after immutable history INSERT;
    // both statements must roll back together.
    sqlx::query(
        "CREATE FUNCTION orbit_test_fail_availability_current() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'synthetic current-pointer failure'; END $$",
    )
    .execute(&fixture.engine.pool)
    .await?;
    sqlx::query(
        "CREATE TRIGGER orbit_test_fail_availability_current BEFORE INSERT OR UPDATE ON orbit_availability_current FOR EACH ROW EXECUTE FUNCTION orbit_test_fail_availability_current()",
    )
    .execute(&fixture.engine.pool)
    .await?;
    let second_capture = orbit::provider_status::antigravity_usage_capture(&raw)?;
    let second = second_capture.availability_snapshot(credential.clone(), 2_000, 10_000)?;
    assert!(store.record(&second).await.is_err());
    let history_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM orbit_availability_snapshots")
            .fetch_one(&fixture.engine.pool)
            .await?;
    assert_eq!(history_count, 1);
    assert_eq!(store.current_for(&resource).await?, vec![snapshot]);
    sqlx::query("DROP TRIGGER orbit_test_fail_availability_current ON orbit_availability_current")
        .execute(&fixture.engine.pool)
        .await?;
    sqlx::query("DROP FUNCTION orbit_test_fail_availability_current()")
        .execute(&fixture.engine.pool)
        .await?;
    Ok(())
}
