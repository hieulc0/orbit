use orbit::availability::{
    AvailabilityScope, AvailabilitySnapshot, AvailabilityState, CredentialIdentity,
    EvidenceConfidence, EvidenceSource, ExecutionResourceIdentity, QuotaWindow, RuntimeIdentity,
    effective_at,
};

fn resource() -> ExecutionResourceIdentity {
    ExecutionResourceIdentity {
        runtime: RuntimeIdentity {
            family: "acp".into(),
            adapter: "codex_bridge".into(),
            binding: "worker-codex".into(),
            image_digest: format!("sha256:{}", "a".repeat(64)),
            agent_revision: "0.156.0".into(),
            adapter_version: "1".into(),
        },
        credential: CredentialIdentity {
            provider: "example".into(),
            reference: "coding-account".into(),
            generation: "2".into(),
            catalog_id: None,
        },
        model: "provider/model".into(),
        reasoning_effort: Some("high".into()),
    }
}

#[test]
fn catalog_identity_and_scope_keys_survive_reference_rename() {
    let mut before = resource();
    before.credential.catalog_id = Some("00000000-0000-4000-8000-000000000001".into());
    let mut after = before.clone();
    after.credential.reference = "antigravity-weedy".into();
    assert_eq!(
        AvailabilityScope::Credential(before.credential.clone())
            .key()
            .unwrap(),
        AvailabilityScope::Credential(after.credential.clone())
            .key()
            .unwrap()
    );
    assert!(AvailabilityScope::Credential(before.credential.clone()).matches(&after));

    let mut another_generation = after.clone();
    another_generation.credential.generation = "3".into();
    assert_ne!(
        AvailabilityScope::Credential(before.credential.clone())
            .key()
            .unwrap(),
        AvailabilityScope::Credential(another_generation.credential.clone())
            .key()
            .unwrap()
    );
    assert!(!AvailabilityScope::Credential(before.credential).matches(&another_generation));
}

fn snapshot(
    scope: AvailabilityScope,
    state: AvailabilityState,
    observed: i64,
    expires: i64,
) -> AvailabilitySnapshot {
    AvailabilitySnapshot {
        applies_to: scope,
        observed_at_ms: observed,
        expires_at_ms: expires,
        state,
        quota_windows: vec![],
        quota_buckets: vec![],
        quota_groups: vec![],
        source: EvidenceSource::ExecutionResult,
        confidence: EvidenceConfidence::ExecutionObserved,
        source_revision: "adapter-v1".into(),
        evidence_digest: format!("sha256:{}", "b".repeat(64)),
        provider_observed_at_ms: None,
        provider_status_observation: None,
    }
}

#[test]
fn resource_identity_is_versioned_and_sensitive_to_each_dimension() {
    let base = resource();
    let id = base.id().unwrap();
    let mut changed = base.clone();
    changed.runtime.image_digest = format!("sha256:{}", "c".repeat(64));
    assert_ne!(id, changed.id().unwrap());
    changed = base.clone();
    changed.credential.generation = "3".into();
    assert_ne!(id, changed.id().unwrap());
    changed = base.clone();
    changed.model = "other/model".into();
    assert_ne!(id, changed.id().unwrap());
    changed = base.clone();
    changed.reasoning_effort = None;
    assert_ne!(id, changed.id().unwrap());
    changed = base;
    changed.credential.reference = "/home/user/.secret".into();
    assert!(changed.id().is_err());
}

#[test]
fn unknown_quota_fields_stay_absent() {
    let mut evidence = snapshot(
        AvailabilityScope::Exact(resource()),
        AvailabilityState::QuotaExhausted,
        10,
        20,
    );
    evidence.quota_windows.push(QuotaWindow {
        label: "rolling-window".into(),
        duration_minutes: None,
        used_percent: None,
        remaining_percent: None,
        resets_at_ms: None,
        exhausted: Some(true),
    });
    let value = serde_json::to_value(&evidence).unwrap();
    let window = &value["quota_windows"][0];
    assert!(window.get("used_percent").is_none());
    assert!(window.get("remaining_percent").is_none());
    assert!(window.get("resets_at_ms").is_none());
    assert_eq!(window["exhausted"], true);
    assert_eq!(
        serde_json::from_value::<AvailabilitySnapshot>(value).unwrap(),
        evidence
    );
}

#[test]
fn legacy_flat_snapshot_deserializes_without_changing_its_serialized_shape() {
    let evidence = snapshot(
        AvailabilityScope::Credential(resource().credential),
        AvailabilityState::Ready,
        10,
        20,
    );
    let mut legacy = serde_json::to_value(&evidence).unwrap();
    legacy.as_object_mut().unwrap().remove("quota_buckets");
    legacy.as_object_mut().unwrap().remove("quota_groups");
    let decoded: AvailabilitySnapshot = serde_json::from_value(legacy).unwrap();
    assert_eq!(decoded, evidence);
    assert!(decoded.quota_buckets.is_empty());
    assert!(decoded.quota_groups.is_empty());
    assert_eq!(decoded.id().unwrap(), evidence.id().unwrap());
    assert_eq!(
        serde_json::to_value(&decoded).unwrap().get("quota_buckets"),
        None
    );
    assert_eq!(
        serde_json::to_value(&decoded).unwrap().get("quota_groups"),
        None
    );
}

#[test]
fn bucket_can_carry_a_provider_defined_narrower_scope() {
    use orbit::availability::{QuotaBucket, QuotaBucketScope, QuotaBucketWindow};

    let mut evidence = snapshot(
        AvailabilityScope::Credential(resource().credential),
        AvailabilityState::Unknown,
        10,
        20,
    );
    evidence.quota_buckets.push(QuotaBucket {
        provider_bucket_fingerprint: format!("qb1:{}", "a".repeat(64)),
        provider_label: None,
        scope: Some(QuotaBucketScope::ModelGroup {
            fingerprint: format!("mg1:{}", "b".repeat(64)),
        }),
        windows: vec![QuotaBucketWindow {
            provider_window_id: "primary".into(),
            duration_minutes: None,
            used_percent: Some(26.0),
            remaining_percent: None,
            remaining_fraction: None,
            resets_at_ms: None,
            provider_reset_time: None,
            exhausted: None,
        }],
    });
    evidence.validate().unwrap();
    assert_eq!(
        serde_json::from_value::<AvailabilitySnapshot>(serde_json::to_value(&evidence).unwrap())
            .unwrap(),
        evidence
    );
}

#[test]
fn broad_positive_does_not_prove_model_ready_and_negative_is_scoped() {
    let r = resource();
    let broad = snapshot(
        AvailabilityScope::Credential(r.credential.clone()),
        AvailabilityState::Ready,
        10,
        100,
    );
    assert_eq!(
        effective_at(&r, &[broad], 20).unwrap().state,
        AvailabilityState::Unknown
    );
    let ready = snapshot(
        AvailabilityScope::Exact(r.clone()),
        AvailabilityState::Ready,
        11,
        100,
    );
    assert_eq!(
        effective_at(&r, std::slice::from_ref(&ready), 20)
            .unwrap()
            .state,
        AvailabilityState::Ready
    );
    let negative = snapshot(
        AvailabilityScope::CredentialModel {
            credential: r.credential.clone(),
            model: r.model.clone(),
        },
        AvailabilityState::QuotaExhausted,
        12,
        90,
    );
    assert_eq!(
        effective_at(&r, &[ready.clone(), negative.clone()], 20)
            .unwrap()
            .state,
        AvailabilityState::QuotaExhausted
    );
    let mut other = r.clone();
    other.model = "different/model".into();
    assert_eq!(
        effective_at(&other, std::slice::from_ref(&negative), 20)
            .unwrap()
            .state,
        AvailabilityState::Unknown
    );
    let decision = effective_at(&r, &[ready, negative], 100).unwrap();
    assert_eq!(decision.state, AvailabilityState::Unknown);
    assert_eq!(decision.stale_ids.len(), 2);
    let future = snapshot(
        AvailabilityScope::Exact(r.clone()),
        AvailabilityState::Ready,
        120,
        200,
    );
    assert_eq!(
        effective_at(&r, &[future], 110).unwrap().state,
        AvailabilityState::Unknown
    );
}

#[test]
fn newer_scope_evidence_wins_regardless_of_input_order() {
    let r = resource();
    let old = snapshot(
        AvailabilityScope::Exact(r.clone()),
        AvailabilityState::Ready,
        10,
        100,
    );
    let new = snapshot(
        AvailabilityScope::Exact(r.clone()),
        AvailabilityState::RateLimited,
        20,
        100,
    );
    let forward = effective_at(&r, &[old.clone(), new.clone()], 30).unwrap();
    let reverse = effective_at(&r, &[new, old], 30).unwrap();
    assert_eq!(forward, reverse);
    assert_eq!(forward.state, AvailabilityState::RateLimited);
    assert_eq!(forward.evidence_ids.len(), 1);
}

#[test]
fn malformed_evidence_and_optimistic_override_are_rejected() {
    let mut evidence = snapshot(
        AvailabilityScope::Exact(resource()),
        AvailabilityState::Ready,
        10,
        20,
    );
    evidence.source = EvidenceSource::OperatorOverride;
    evidence.confidence = EvidenceConfidence::OperatorAsserted;
    assert!(evidence.validate().is_err());
    evidence.source = EvidenceSource::ProviderNativeStatus;
    evidence.confidence = EvidenceConfidence::AuthoritativeNative;
    evidence.quota_windows.push(QuotaWindow {
        label: "rolling".into(),
        duration_minutes: None,
        used_percent: Some(f64::NAN),
        remaining_percent: None,
        resets_at_ms: None,
        exhausted: None,
    });
    assert!(evidence.validate().is_err());
}
