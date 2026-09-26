use orbit::availability::{
    AvailabilityScope, AvailabilityState, CredentialIdentity, EvidenceConfidence,
    ExecutionResourceIdentity, ProviderScopeEvidenceState, QuotaEvidencePromotion, RuntimeIdentity,
    effective_at,
};
use orbit::continuation::TerminationReason;
use orbit::provider_status::{
    CodexRateLimitObservationState, antigravity_usage_capture, codex_rate_limits_observation,
    codex_rate_limits_snapshot, execution_result_snapshot,
};
use serde_json::json;
use std::collections::BTreeSet;

fn resource() -> ExecutionResourceIdentity {
    ExecutionResourceIdentity {
        runtime: RuntimeIdentity {
            family: "acp".into(),
            adapter: "codex_bridge".into(),
            binding: "codex-fixture".into(),
            image_digest: format!("sha256:{}", "a".repeat(64)),
            agent_revision: "0.156.0".into(),
            adapter_version: "1".into(),
        },
        credential: CredentialIdentity {
            provider: "openai".into(),
            reference: "operator-account".into(),
            generation: "1".into(),
            catalog_id: None,
        },
        model: "gpt-6-luna".into(),
        reasoning_effort: Some("high".into()),
    }
}

fn antigravity_credential() -> CredentialIdentity {
    CredentialIdentity {
        provider: "antigravity".into(),
        reference: "synthetic-status-fixture".into(),
        generation: "1".into(),
        catalog_id: Some("00000000-0000-4000-8000-000000000001".into()),
    }
}

fn antigravity_fixture() -> serde_json::Value {
    json!({
        "command": {"data": {"groups": [
            {"buckets": [
                {"id":"opaque-test-id-a", "name":"synthetic-label-a", "window":"primary",
                 "remaining_fraction":0.42, "reset_time":"2026-01-02T03:04:05Z",
                 "future_bucket_note":"synthetic-unknown-value"},
                {"id":"opaque-test-id-a", "name":"synthetic-label-a", "window":"secondary",
                 "remaining_fraction":null, "reset_time":null}
            ]},
            {"buckets": [
                {"id":"opaque-test-id-b", "name":"synthetic-label-b", "window":"5h",
                 "remaining_fraction":0.75},
                {"id":"opaque-test-id-c", "window":"synthetic-window-c",
                 "remaining_fraction":null, "reset_time":null}
            ]}
        ]}},
        "conversation_id":"",
        "duration_seconds":0,
        "num_turns":0,
        "response":"synthetic-response-value",
        "status":"synthetic-status-value",
        "usage":{"input_tokens":987654, "output_tokens":123, "total_tokens":987777,
                 "cache_read_tokens":2, "thinking_tokens":3},
        "account":{"access_token":"synthetic-secret-value"}
    })
}

fn antigravity_group_fixture() -> serde_json::Value {
    json!({
        "command": {"data": {"groups": [
            {
                "id": "synthetic-unreviewed-group-id-gemini",
                "name": "Gemini Models",
                "description": "Models within this group: Gemini Flash, Gemini Pro",
                "models": ["synthetic-unreviewed-member-value"],
                "buckets": [
                    {"id":"synthetic-gemini-weekly", "window":"weekly",
                     "remaining_fraction":0.2417, "reset_time":"2026-09-26T03:04:05Z"},
                    {"id":"synthetic-gemini-five-hour", "window":"5h",
                     "remaining_fraction":1.0, "reset_time":null}
                ],
                "future_private_note": "synthetic-unknown-group-value"
            },
            {
                "id": "synthetic-unreviewed-group-id-claude-gpt",
                "name": "Claude and GPT Models",
                "description": "Models within this group: Claude Opus, Claude Sonnet, GPT-OSS",
                "future_private_note": "synthetic-unknown-group-value",
                "buckets": [
                    {"id":"synthetic-claude-weekly", "window":"weekly",
                     "remaining_fraction":1.0, "reset_time":null},
                    {"id":"synthetic-claude-five-hour", "window":"5h",
                     "remaining_fraction":1.0, "reset_time":null}
                ]
            }
        ]}}
    })
}

#[test]
fn antigravity_usage_extracts_only_context_approved_bucket_values() {
    let raw = serde_json::to_vec(&antigravity_fixture()).unwrap();
    let capture = antigravity_usage_capture(&raw).unwrap();
    assert!(capture.normalization_ready);
    assert_eq!(capture.summary.top_level_fields.len(), 8);
    let quota_shape = capture.summary.quota.as_ref().unwrap();
    assert_eq!(quota_shape.groups, 2);
    assert_eq!(quota_shape.buckets_per_group, [2, 2]);
    assert!(
        quota_shape
            .observed_bucket_fields
            .contains(&"remaining_fraction".to_owned())
    );
    assert_eq!(quota_shape.additional_bucket_field_count, 1);
    assert_eq!(capture.quota_buckets.len(), 3);
    let bucket_a = capture
        .quota_buckets
        .iter()
        .find(|bucket| bucket.windows.len() == 2)
        .unwrap();
    assert_eq!(bucket_a.windows.len(), 2);
    assert_eq!(
        bucket_a
            .windows
            .iter()
            .map(|window| window.provider_window_id.as_str())
            .collect::<Vec<_>>(),
        ["primary", "secondary"]
    );
    assert_eq!(bucket_a.windows[0].remaining_fraction, Some(0.42));
    assert_eq!(
        bucket_a.windows[0].provider_reset_time.as_deref(),
        Some("2026-01-02T03:04:05Z")
    );
    assert_eq!(bucket_a.windows[0].resets_at_ms, Some(1_767_323_045_000));
    assert_eq!(bucket_a.windows[1].remaining_fraction, None);
    assert_eq!(bucket_a.windows[1].resets_at_ms, None);

    let status = capture
        .availability_snapshot(antigravity_credential(), 10_000, 20_000)
        .unwrap();
    assert_eq!(status.state, AvailabilityState::Unknown);
    assert_eq!(status.observed_at_ms, 10_000);
    assert_eq!(status.expires_at_ms, 20_000);
    assert_eq!(status.provider_observed_at_ms, None);
    assert!(matches!(
        status.applies_to,
        AvailabilityScope::Credential(_)
    ));
    assert!(status.quota_windows.is_empty());
    assert_eq!(status.quota_buckets.len(), 3);

    let safe = serde_json::to_string(&capture).unwrap();
    for forbidden in [
        "synthetic-unreviewed-group-id-gemini",
        "synthetic-unreviewed-group-id-claude-gpt",
        "opaque-test-id-a",
        "opaque-test-id-b",
        "opaque-test-id-c",
        "synthetic-label-a",
        "synthetic-unknown-value",
        "synthetic-response-value",
        "synthetic-status-value",
        "synthetic-secret-value",
        "987654",
    ] {
        assert!(
            !safe.contains(forbidden),
            "unexpected retained value: {forbidden}"
        );
    }
    assert!(safe.contains("<REDACTED>"));
    assert!(
        capture
            .schema
            .rejected_fields
            .iter()
            .any(|field| field.path == "$.account.access_token")
    );
    assert!(
        capture
            .extraction_diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.reason == "group-name-field-missing" })
    );
    assert!(!capture.group_metadata_ready);
    assert!(capture.quota_groups.is_empty());
}

#[test]
fn antigravity_bucket_fingerprints_are_stable_and_schema_summary_is_compact() {
    let fixture = antigravity_fixture();
    let raw = serde_json::to_vec(&fixture).unwrap();
    let first = antigravity_usage_capture(&raw).unwrap();
    let second = antigravity_usage_capture(&raw).unwrap();
    assert_eq!(first.summary, second.summary);
    assert_eq!(first.quota_buckets, second.quota_buckets);
    assert!(first.quota_buckets.iter().all(|bucket| {
        bucket.provider_bucket_fingerprint.starts_with("qb1:")
            && bucket.provider_bucket_fingerprint.len() == 68
    }));
    let summary = serde_json::to_vec(&first.summary).unwrap();
    assert!(summary.len() < 8_192);
    let rendered = String::from_utf8(summary).unwrap();
    assert!(rendered.contains("buckets_per_group"));
    assert!(rendered.contains("schema_fingerprint"));
    assert!(!rendered.contains("opaque-test-id-a"));

    let mut changed = fixture;
    changed["command"]["data"]["groups"][0]["buckets"][0]["id"] = json!("different-synthetic-id");
    let changed = antigravity_usage_capture(&serde_json::to_vec(&changed).unwrap()).unwrap();
    let first_fingerprints = first
        .quota_buckets
        .iter()
        .map(|bucket| bucket.provider_bucket_fingerprint.as_str())
        .collect::<BTreeSet<_>>();
    let changed_fingerprints = changed
        .quota_buckets
        .iter()
        .map(|bucket| bucket.provider_bucket_fingerprint.as_str())
        .collect::<BTreeSet<_>>();
    assert_ne!(first_fingerprints, changed_fingerprints);
}

#[test]
fn antigravity_provider_groups_preserve_labels_and_link_existing_qb1_buckets() {
    let mut fixture = antigravity_group_fixture();
    fixture["name"] = json!("synthetic-unreviewed-root-name");
    fixture["description"] = json!("synthetic-unreviewed-root-description");
    let capture = antigravity_usage_capture(&serde_json::to_vec(&fixture).unwrap()).unwrap();
    assert!(capture.normalization_ready);
    assert!(capture.group_metadata_ready);
    assert!(capture.membership_ready);
    assert_eq!(capture.quota_groups.len(), 2);
    assert_eq!(capture.quota_buckets.len(), 4);

    let gemini = capture
        .quota_groups
        .iter()
        .find(|group| group.provider_display_name.as_deref() == Some("Gemini Models"))
        .unwrap();
    assert_eq!(gemini.fingerprint.len(), 68);
    assert!(gemini.fingerprint.starts_with("qg1:"));
    assert_eq!(gemini.bucket_fingerprints.len(), 2);
    assert_eq!(
        gemini
            .members
            .iter()
            .map(|member| member.provider_label.as_str())
            .collect::<Vec<_>>(),
        ["Gemini Flash", "Gemini Pro"]
    );
    assert_eq!(
        gemini.provider_description.as_deref(),
        Some("Models within this group: Gemini Flash, Gemini Pro")
    );

    let claude = capture
        .quota_groups
        .iter()
        .find(|group| group.provider_display_name.as_deref() == Some("Claude and GPT Models"))
        .unwrap();
    assert_eq!(
        claude
            .members
            .iter()
            .map(|member| member.provider_label.as_str())
            .collect::<Vec<_>>(),
        ["Claude Opus", "Claude Sonnet", "GPT-OSS"]
    );
    assert_eq!(claude.bucket_fingerprints.len(), 2);

    for group in &capture.quota_groups {
        for fingerprint in &group.bucket_fingerprints {
            let bucket = capture
                .quota_buckets
                .iter()
                .find(|bucket| &bucket.provider_bucket_fingerprint == fingerprint)
                .unwrap();
            assert_eq!(bucket.windows.len(), 1);
            assert!(matches!(
                bucket.windows[0].provider_window_id.as_str(),
                "weekly" | "5h"
            ));
        }
    }
    let snapshot = capture
        .availability_snapshot(antigravity_credential(), 10_000, 20_000)
        .unwrap();
    assert_eq!(snapshot.state, AvailabilityState::Unknown);
    assert_eq!(snapshot.quota_groups, capture.quota_groups);
    assert_eq!(snapshot.quota_buckets, capture.quota_buckets);
    let encoded_snapshot = serde_json::to_vec(&snapshot).unwrap();
    assert_eq!(
        serde_json::from_slice::<orbit::availability::AvailabilitySnapshot>(&encoded_snapshot)
            .unwrap(),
        snapshot
    );

    let encoded = serde_json::to_string(&capture).unwrap();
    for forbidden in [
        "synthetic-gemini-weekly",
        "synthetic-claude-five-hour",
        "synthetic-unknown-group-value",
        "synthetic-unreviewed-member-value",
        "synthetic-unreviewed-root-name",
        "synthetic-unreviewed-root-description",
    ] {
        assert!(!encoded.contains(forbidden), "leaked {forbidden}");
    }
    assert!(encoded.contains("Gemini Models"));
    assert!(encoded.contains("Claude and GPT Models"));
    let group_shape = &capture
        .summary
        .quota
        .as_ref()
        .unwrap()
        .observed_group_fields;
    assert!(group_shape.iter().any(|field| field.field == "description"));
    assert!(group_shape.iter().any(|field| field.field == "name"));
}

#[test]
fn antigravity_group_identity_is_order_independent_and_keeps_qb1_unchanged() {
    let original = antigravity_group_fixture();
    let first = antigravity_usage_capture(&serde_json::to_vec(&original).unwrap()).unwrap();
    let mut without_group_metadata = original.clone();
    for group in without_group_metadata["command"]["data"]["groups"]
        .as_array_mut()
        .unwrap()
    {
        let object = group.as_object_mut().unwrap();
        for key in ["name", "description", "future_private_note"] {
            object.remove(key);
        }
    }
    let old_capture =
        antigravity_usage_capture(&serde_json::to_vec(&without_group_metadata).unwrap()).unwrap();
    assert_eq!(first.quota_buckets, old_capture.quota_buckets);

    let mut reordered = original;
    let groups = reordered["command"]["data"]["groups"]
        .as_array_mut()
        .unwrap();
    groups.reverse();
    for group in groups {
        let description = group["description"].as_str().unwrap();
        let mut members = description
            .strip_prefix("Models within this group: ")
            .unwrap()
            .split(", ")
            .collect::<Vec<_>>();
        members.reverse();
        group["description"] = json!(format!("Models within this group: {}", members.join(", ")));
    }
    let second = antigravity_usage_capture(&serde_json::to_vec(&reordered).unwrap()).unwrap();
    let first_ids = first
        .quota_groups
        .iter()
        .map(|group| group.fingerprint.as_str())
        .collect::<BTreeSet<_>>();
    let second_ids = second
        .quota_groups
        .iter()
        .map(|group| group.fingerprint.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(first_ids, second_ids);
    assert_eq!(first.quota_buckets, second.quota_buckets);
}

#[test]
fn antigravity_group_metadata_missing_or_ambiguous_fails_closed() {
    let base = antigravity_group_fixture();
    let mut missing_name = base.clone();
    missing_name["command"]["data"]["groups"][0]
        .as_object_mut()
        .unwrap()
        .remove("name");
    let capture = antigravity_usage_capture(&serde_json::to_vec(&missing_name).unwrap()).unwrap();
    assert!(capture.normalization_ready);
    assert!(!capture.group_metadata_ready);
    assert!(!capture.membership_ready);
    assert!(
        capture
            .extraction_diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.reason == "group-name-field-missing" })
    );

    for description in [
        "Quota group membership changed",
        "Models within this group: Gemini Flash, ",
        "Models within this group: Gemini Flash, Gemini Flash",
        "Models within this group:",
    ] {
        let mut fixture = base.clone();
        fixture["command"]["data"]["groups"][0]["description"] = json!(description);
        let capture = antigravity_usage_capture(&serde_json::to_vec(&fixture).unwrap()).unwrap();
        assert!(capture.normalization_ready);
        assert!(capture.group_metadata_ready);
        assert!(!capture.membership_ready);
        assert_eq!(capture.quota_groups.len(), 2);
        let gemini = capture
            .quota_groups
            .iter()
            .find(|group| group.provider_display_name.as_deref() == Some("Gemini Models"))
            .unwrap();
        assert!(gemini.members.is_empty());
        assert_eq!(gemini.bucket_fingerprints.len(), 2);
        assert!(
            capture
                .extraction_diagnostics
                .iter()
                .any(|diagnostic| { diagnostic.path == "$.command.data.groups[0].description" })
        );
    }
}

#[test]
fn antigravity_missing_description_keeps_name_and_bucket_metadata_without_members() {
    let mut fixture = antigravity_group_fixture();
    fixture["command"]["data"]["groups"][0]["description"] = serde_json::Value::Null;
    let capture = antigravity_usage_capture(&serde_json::to_vec(&fixture).unwrap()).unwrap();
    assert!(capture.group_metadata_ready);
    assert!(!capture.membership_ready);
    let gemini = capture
        .quota_groups
        .iter()
        .find(|group| group.provider_display_name.as_deref() == Some("Gemini Models"))
        .unwrap();
    assert_eq!(
        gemini.identity_basis,
        orbit::availability::ProviderQuotaGroupIdentityBasis::DisplayNameAndMemberSet
    );
    assert!(gemini.members.is_empty());
    assert_eq!(gemini.provider_description, None);
}

#[test]
fn antigravity_group_fingerprint_ignores_group_and_member_order() {
    let mut first_fixture = antigravity_group_fixture();
    let first = antigravity_usage_capture(&serde_json::to_vec(&first_fixture).unwrap()).unwrap();
    assert!(first.group_metadata_ready);
    assert!(first.membership_ready);
    assert!(first.quota_groups.iter().all(|group| {
        group.identity_basis
            == orbit::availability::ProviderQuotaGroupIdentityBasis::DisplayNameAndMemberSet
    }));

    first_fixture["command"]["data"]["groups"]
        .as_array_mut()
        .unwrap()
        .reverse();
    for group in first_fixture["command"]["data"]["groups"]
        .as_array_mut()
        .unwrap()
    {
        let description = group["description"].as_str().unwrap();
        let mut members = description
            .strip_prefix("Models within this group: ")
            .unwrap()
            .split(", ")
            .collect::<Vec<_>>();
        members.reverse();
        group["description"] = json!(format!("Models within this group: {}", members.join(", ")));
    }
    let second = antigravity_usage_capture(&serde_json::to_vec(&first_fixture).unwrap()).unwrap();
    for group in &first.quota_groups {
        assert!(
            second
                .quota_groups
                .iter()
                .any(|other| { other.fingerprint == group.fingerprint })
        );
    }
}

#[test]
fn antigravity_group_description_parser_is_contextual_bounded_and_conservative() {
    let baseline_fixture = antigravity_group_fixture();
    let baseline =
        antigravity_usage_capture(&serde_json::to_vec(&baseline_fixture).unwrap()).unwrap();
    let baseline_gemini = baseline
        .quota_groups
        .iter()
        .find(|group| group.provider_display_name.as_deref() == Some("Gemini Models"))
        .unwrap();

    let mut whitespace_fixture = baseline_fixture.clone();
    whitespace_fixture["command"]["data"]["groups"][0]["description"] =
        json!("Models within this group:   Gemini Flash  ,  Gemini Pro   ");
    let whitespace =
        antigravity_usage_capture(&serde_json::to_vec(&whitespace_fixture).unwrap()).unwrap();
    let whitespace_gemini = whitespace
        .quota_groups
        .iter()
        .find(|group| group.provider_display_name.as_deref() == Some("Gemini Models"))
        .unwrap();
    assert_eq!(whitespace_gemini.fingerprint, baseline_gemini.fingerprint);
    assert_eq!(
        whitespace_gemini
            .members
            .iter()
            .map(|member| member.provider_label.as_str())
            .collect::<Vec<_>>(),
        ["Gemini Flash", "Gemini Pro"]
    );

    let too_many_members = (0..33)
        .map(|index| format!("Model{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let invalid_descriptions = [
        "models within this group: Gemini Flash, Gemini Pro".to_owned(),
        "Models within this group: Gemini Flash,, Gemini Pro".to_owned(),
        "Models within this group: Gemini Flash, Gemini Flash".to_owned(),
        format!("Models within this group: {too_many_members}"),
        format!("Models within this group: {}", "X".repeat(129)),
        "Models within this group: ".to_owned(),
        "x".repeat(513),
    ];
    for description in invalid_descriptions {
        let mut fixture = baseline_fixture.clone();
        fixture["command"]["data"]["groups"][0]["description"] = json!(description);
        let capture = antigravity_usage_capture(&serde_json::to_vec(&fixture).unwrap()).unwrap();
        assert!(capture.normalization_ready);
        assert!(capture.group_metadata_ready);
        assert!(!capture.membership_ready);
        let gemini = capture
            .quota_groups
            .iter()
            .find(|group| group.provider_display_name.as_deref() == Some("Gemini Models"))
            .unwrap();
        assert!(gemini.members.is_empty());
        assert_eq!(gemini.bucket_fingerprints.len(), 2);
        assert!(capture.extraction_diagnostics.iter().any(|diagnostic| {
            diagnostic.path == "$.command.data.groups[0].description"
                && diagnostic.value == "<REDACTED>"
        }));
    }

    let mut invalid_name = baseline_fixture;
    invalid_name["command"]["data"]["groups"][0]["name"] = json!("N".repeat(129));
    let capture = antigravity_usage_capture(&serde_json::to_vec(&invalid_name).unwrap()).unwrap();
    assert!(!capture.group_metadata_ready);
    assert!(!capture.membership_ready);
    assert!(capture.extraction_diagnostics.iter().any(|diagnostic| {
        diagnostic.path == "$.command.data.groups[0].name"
            && diagnostic.reason == "group-name-has-invalid-type-or-shape"
    }));
}

#[test]
fn antigravity_status_scope_never_treats_bucket_ids_as_account_identity() {
    let capture =
        antigravity_usage_capture(&serde_json::to_vec(&antigravity_fixture()).unwrap()).unwrap();
    let snapshot = capture
        .availability_snapshot(antigravity_credential(), 10, 20)
        .unwrap();
    let encoded = serde_json::to_string(&snapshot).unwrap();
    assert!(encoded.contains("catalog_id"));
    assert!(!encoded.contains("opaque-test-id-a"));
    assert!(!encoded.contains("provider_scope"));
    assert_eq!(snapshot.state, AvailabilityState::Unknown);
}

#[test]
fn antigravity_reset_time_is_strict_and_missing_values_stay_missing() {
    let mut fixture = json!({"command":{"data":{"groups":[{"buckets":[
        {"id":"synthetic-a","window":"5h","remaining_fraction":0.5,"reset_time":"not-a-time"},
        {"id":"synthetic-b","window":null,"remaining_fraction":null,"reset_time":null},
        {"id":"synthetic-c","window":"weekly","remaining_fraction":null}
    ]}]}}});
    fixture["metadata"] = json!({"reset_time":"2099-12-31T23:59:59Z"});
    let capture = antigravity_usage_capture(&serde_json::to_vec(&fixture).unwrap()).unwrap();
    assert!(capture.normalization_ready);
    let bucket_a = capture
        .quota_buckets
        .iter()
        .find(|bucket| {
            bucket
                .windows
                .iter()
                .any(|window| window.remaining_fraction == Some(0.5))
        })
        .unwrap();
    assert_eq!(bucket_a.windows[0].remaining_fraction, Some(0.5));
    assert_eq!(bucket_a.windows[0].provider_reset_time, None);
    assert_eq!(bucket_a.windows[0].resets_at_ms, None);
    assert!(capture.extraction_diagnostics.iter().any(|diagnostic| {
        diagnostic.path.ends_with(".reset_time")
            && diagnostic.reason == "timestamp-format-or-value-unsupported"
            && diagnostic.value == "<REDACTED>"
    }));
    assert!(
        !serde_json::to_string(&capture)
            .unwrap()
            .contains("2099-12-31")
    );
}

#[test]
fn antigravity_malformed_fraction_or_window_blocks_the_whole_observation() {
    assert!(antigravity_usage_capture(b"{malformed synthetic fixture}").is_err());
    for invalid in [json!("0.5"), json!(true), json!({"fraction":0.5})] {
        let fixture = json!({"command":{"data":{"groups":[{"buckets":[
            {"id":"synthetic-id","window":"5h","remaining_fraction":invalid}
        ]}]}}});
        let capture = antigravity_usage_capture(&serde_json::to_vec(&fixture).unwrap()).unwrap();
        assert!(!capture.normalization_ready);
        assert!(capture.quota_buckets.is_empty());
        assert_eq!(capture.summary.quota.as_ref().unwrap().groups, 1);
        assert_eq!(
            capture.summary.quota.as_ref().unwrap().buckets_per_group,
            [1]
        );
        assert_eq!(capture.extraction_diagnostics[0].value, "<REDACTED>");
        assert!(
            capture
                .availability_snapshot(antigravity_credential(), 10, 20)
                .is_err()
        );
    }
    let fixture = json!({"command":{"data":{"groups":[{"buckets":[
        {"id":"synthetic-id","window":5,"remaining_fraction":0.5}
    ]}]}}});
    let capture = antigravity_usage_capture(&serde_json::to_vec(&fixture).unwrap()).unwrap();
    assert!(!capture.normalization_ready);
    assert!(capture.quota_buckets.is_empty());
}

#[test]
fn antigravity_finite_fraction_is_preserved_without_invented_range_or_clamping() {
    let fixture = json!({"command":{"data":{"groups":[{"buckets":[
        {"id":"synthetic-range-id","window":"provider-window","remaining_fraction":1.25}
    ]}]}}});
    let capture = antigravity_usage_capture(&serde_json::to_vec(&fixture).unwrap()).unwrap();
    assert!(capture.normalization_ready);
    assert_eq!(
        capture.quota_buckets[0].windows[0].remaining_fraction,
        Some(1.25)
    );
    assert_eq!(
        capture
            .availability_snapshot(antigravity_credential(), 10, 20)
            .unwrap()
            .state,
        AvailabilityState::Unknown
    );
}

#[test]
fn antigravity_bucket_identity_collision_between_groups_fails_closed() {
    let fixture = json!({"command":{"data":{"groups":[
        {"buckets":[{"id":"synthetic-duplicate","window":"primary","remaining_fraction":0.5}]},
        {"buckets":[{"id":"synthetic-duplicate","window":"secondary","remaining_fraction":0.4}]}
    ]}}});
    let capture = antigravity_usage_capture(&serde_json::to_vec(&fixture).unwrap()).unwrap();
    assert!(!capture.normalization_ready);
    assert!(capture.quota_buckets.is_empty());
    assert!(
        capture.extraction_diagnostics.iter().any(|diagnostic| {
            diagnostic.reason == "quota-bucket-identity-collides-across-groups"
        })
    );
    assert!(
        !serde_json::to_string(&capture)
            .unwrap()
            .contains("synthetic-duplicate")
    );
}

fn parse(input: serde_json::Value) -> orbit::availability::AvailabilitySnapshot {
    codex_rate_limits_snapshot(
        &resource(),
        "fixture-account-id",
        &serde_json::to_vec(&input).unwrap(),
        10_000,
        20_000,
    )
    .unwrap()
}

#[test]
fn codex_versioned_structured_status_preserves_windows_without_model_claims() {
    let status = parse(json!({
        "accountId":"fixture-account-id",
        "ordinaryUsageAllowed":true,
        "rateLimits":{"primary":null,"secondary":null},
        "rateLimitsByLimitId":{
            "codex":{"limitId":"codex","primary":{"usedPercent":25,"windowDurationMins":15,"resetsAt":1730947200},"secondary":null},
            "codex_other":{"limitId":"codex_other","primary":{"usedPercent":100,"windowDurationMins":null,"resetsAt":null},
                           "secondary":{"usedPercent":0,"windowDurationMins":60,"resetsAt":null}}
        },
        "untrustedPayload":"never-durable-provider-secret"
    }));
    assert_eq!(status.state, AvailabilityState::Ready);
    assert_eq!(status.confidence, EvidenceConfidence::AuthoritativeNative);
    assert!(matches!(
        status.applies_to,
        AvailabilityScope::Credential(_)
    ));
    assert_eq!(status.quota_windows.len(), 3);
    assert_eq!(status.quota_buckets.len(), 2);
    assert_ne!(
        status.quota_buckets[0].provider_bucket_fingerprint,
        status.quota_buckets[1].provider_bucket_fingerprint
    );
    assert!(status.quota_buckets.iter().all(|bucket| {
        bucket.provider_bucket_fingerprint.starts_with("qb1:")
            && bucket.provider_bucket_fingerprint.len() == 68
            && bucket.scope.is_none()
    }));
    assert_eq!(status.quota_buckets[0].windows.len(), 1);
    assert_eq!(
        status.quota_buckets[0].windows[0].provider_window_id,
        "primary"
    );
    assert_eq!(status.quota_buckets[0].windows[0].used_percent, Some(25.0));
    assert_eq!(
        status.quota_buckets[0].windows[0].remaining_percent,
        Some(75.0)
    );
    assert_eq!(
        status.quota_buckets[0].windows[0].remaining_fraction,
        Some(0.75)
    );
    assert_eq!(
        status.quota_buckets[0].windows[0].duration_minutes,
        Some(15)
    );
    assert_eq!(status.quota_buckets[1].windows.len(), 2);
    assert_eq!(
        status.quota_buckets[1]
            .windows
            .iter()
            .map(|window| window.provider_window_id.as_str())
            .collect::<Vec<_>>(),
        ["primary", "secondary"]
    );
    assert_eq!(status.quota_buckets[1].windows[0].used_percent, Some(100.0));
    assert_eq!(status.quota_buckets[1].windows[1].used_percent, Some(0.0));
    assert!(status.quota_windows.iter().any(|w| {
        w.used_percent == Some(25.0)
            && w.duration_minutes == Some(15)
            && w.resets_at_ms == Some(1_730_947_200_000)
            && w.remaining_percent == Some(75.0)
            && w.exhausted.is_none()
    }));
    assert!(status.quota_windows.iter().any(|w| {
        w.used_percent == Some(100.0) && w.exhausted.is_none() && w.resets_at_ms.is_none()
    }));
    let durable = serde_json::to_string(&status).unwrap();
    assert!(!durable.contains("fixture-account-id"));
    assert!(!durable.contains("never-durable-provider-secret"));
    assert!(!durable.contains("codex_other"));
    assert!(!durable.contains("codex\""));
    assert!(!durable.contains("Luna reserve"));
    assert!(durable.contains("provider_bucket_fingerprint"));
    assert!(durable.contains("provider_window_id"));
    assert_eq!(
        serde_json::from_str::<orbit::availability::AvailabilitySnapshot>(&durable).unwrap(),
        status
    );
    assert_eq!(
        effective_at(&resource(), &[status], 11_000).unwrap().state,
        AvailabilityState::Unknown
    );
}

#[test]
fn codex_rate_limit_observation_is_separate_from_scope_promotion() {
    let result = json!({
        "accountId":"synthetic-account-id",
        "ordinaryUsageAllowed":true,
        "rateLimitsByLimitId":{
            "synthetic-limit-a":{
                "limitId":"synthetic-limit-a",
                "limitName":"synthetic plan",
                "primary":{"usedPercent":31,"windowDurationMins":300,"resetsAt":1730947200},
                "secondary":{"usedPercent":46,"windowDurationMins":10080,"resetsAt":1731542400}
            }
        }
    });
    let observed = codex_rate_limits_observation(&resource(), &result);
    assert_eq!(observed.state, CodexRateLimitObservationState::Observed);
    assert_eq!(observed.bucket_count, 1);
    assert_eq!(observed.window_count, 2);
    assert_eq!(
        observed.legacy_map_identity_relation,
        orbit::provider_status::CodexLimitIdentityRelation::MapOnly
    );

    let unconfirmed = codex_rate_limits_snapshot(
        &resource(),
        "",
        &serde_json::to_vec(&result).unwrap(),
        10_000,
        20_000,
    )
    .unwrap();
    assert_eq!(unconfirmed.state, AvailabilityState::Unknown);
    assert_eq!(unconfirmed.quota_buckets.len(), 1);
    assert_eq!(unconfirmed.quota_windows.len(), 2);
    let evidence = unconfirmed.provider_status_observation.as_ref().unwrap();
    assert_eq!(
        evidence.scope_state,
        ProviderScopeEvidenceState::Unconfirmed
    );
    assert_eq!(
        evidence.quota_promotion,
        QuotaEvidencePromotion::ObservedUnconfirmed
    );
    assert_eq!(evidence.ordinary_usage_allowed, Some(true));

    let no_windows = json!({
        "accountId":"synthetic-account-id",
        "rateLimits":{"primary":null,"secondary":null}
    });
    assert_eq!(
        codex_rate_limits_observation(&resource(), &no_windows).state,
        CodexRateLimitObservationState::NoUsableWindows
    );
    let malformed = json!({
        "rateLimits":{"primary":{"usedPercent":"unknown"}}
    });
    assert_eq!(
        codex_rate_limits_observation(&resource(), &malformed).state,
        CodexRateLimitObservationState::UnsupportedShape
    );
}

#[test]
fn codex_map_shape_can_be_normalized_without_legacy_rate_limits_object() {
    let status = parse(json!({
        "accountId":"fixture-account-id",
        "ordinaryUsageAllowed":true,
        "rateLimitsByLimitId":{
            "synthetic-limit":{
                "limitId":"synthetic-limit",
                "primary":{"usedPercent":0,"windowDurationMins":5,"resetsAt":1730947200},
                "secondary":null
            }
        }
    }));
    assert_eq!(status.state, AvailabilityState::Ready);
    assert_eq!(status.quota_buckets.len(), 1);
    assert_eq!(status.quota_windows.len(), 1);
    assert_eq!(status.quota_windows[0].used_percent, Some(0.0));
}

#[test]
fn codex_legacy_and_map_identity_match_is_observed_not_assumed() {
    let combined = json!({
        "accountId":"fixture-account-id",
        "ordinaryUsageAllowed":true,
        "rateLimits":{"limitId":"synthetic-limit","limitName":null,
            "primary":{"usedPercent":0,"windowDurationMins":300,"resetsAt":0},
            "secondary":null},
        "rateLimitsByLimitId":{"synthetic-limit":{"limitId":"synthetic-limit",
            "limitName":"synthetic-provider-label",
            "primary":{"usedPercent":0,"windowDurationMins":300,"resetsAt":0},
            "secondary":null}}
    });
    let observation = codex_rate_limits_observation(&resource(), &combined);
    assert_eq!(
        observation.legacy_map_identity_relation,
        orbit::provider_status::CodexLimitIdentityRelation::ExactMatch
    );
    let combined_snapshot = parse(combined);
    let legacy_only_snapshot = parse(json!({
        "accountId":"fixture-account-id",
        "ordinaryUsageAllowed":true,
        "rateLimits":{"limitId":"synthetic-limit","limitName":null,
            "primary":{"usedPercent":0,"windowDurationMins":300,"resetsAt":0},
            "secondary":null}
    }));
    assert_eq!(combined_snapshot.quota_buckets.len(), 1);
    assert_eq!(legacy_only_snapshot.quota_buckets.len(), 1);
    assert_eq!(
        combined_snapshot.quota_buckets[0].provider_bucket_fingerprint,
        legacy_only_snapshot.quota_buckets[0].provider_bucket_fingerprint
    );
    let window = &combined_snapshot.quota_buckets[0].windows[0];
    assert_eq!(window.used_percent, Some(0.0));
    assert_eq!(window.remaining_percent, Some(100.0));
    assert_eq!(window.remaining_fraction, Some(1.0));
    assert_eq!(window.duration_minutes, Some(300));
    assert_eq!(window.resets_at_ms, Some(0));
    assert_eq!(
        combined_snapshot.quota_buckets[0].provider_label.as_deref(),
        Some("synthetic-provider-label")
    );
    let safe = serde_json::to_string(&combined_snapshot).unwrap();
    assert!(!safe.contains("synthetic-limit"));
}

#[test]
fn codex_0156_observed_rate_limit_schema_is_understood_without_retaining_raw_ids() {
    // Sanitized fixture mirrors the one live-observed shape. Every account,
    // bucket, description, and reset value here is synthetic.
    let result = json!({
        "accountId":"fixture-account-id",
        "ordinaryUsageAllowed":true,
        "rateLimits":{
            "credits":{"balance":"synthetic-balance","hasCredits":true,"unlimited":false},
            "individualLimit":null,
            "limitId":"synthetic-default-limit",
            "limitName":null,
            "normalModelSlug":null,
            "planType":"synthetic-plan",
            "primary":{"usedPercent":12,"windowDurationMins":300,"resetsAt":1730947200},
            "rateLimitReachedType":null,
            "secondary":{"usedPercent":46,"windowDurationMins":10080,"resetsAt":1731542400},
            "spendControlReached":false
        },
        "rateLimitsByLimitId":{
            "synthetic-limit-a":{
                "credits":{"balance":"synthetic-balance","hasCredits":true,"unlimited":false},
                "individualLimit":null,
                "limitId":"synthetic-limit-a",
                "limitName":"synthetic-name-a",
                "normalModelSlug":null,
                "planType":"synthetic-plan",
                "primary":{"usedPercent":25,"windowDurationMins":300,"resetsAt":1730947200},
                "rateLimitReachedType":null,
                "secondary":null,
                "spendControlReached":null
            },
            "synthetic-limit-b":{
                "credits":null,
                "individualLimit":null,
                "limitId":"synthetic-limit-b",
                "limitName":null,
                "normalModelSlug":"synthetic-model-label",
                "planType":"synthetic-plan",
                "primary":{"usedPercent":100,"windowDurationMins":5,"resetsAt":1730947500},
                "rateLimitReachedType":null,
                "secondary":{"usedPercent":0,"windowDurationMins":60,"resetsAt":1730950800},
                "spendControlReached":false
            }
        },
        "rateLimitResetCredits":{
            "availableCount":0,
            "credits":[{
                "description":"synthetic credit",
                "expiresAt":1731000000,
                "grantedAt":1730900000,
                "id":"synthetic-credit-id",
                "resetType":"synthetic-reset",
                "status":"synthetic-status",
                "title":"synthetic-title"
            }]
        },
        "rateLimitUpsell":null
    });

    let observed = codex_rate_limits_observation(&resource(), &result);
    assert_eq!(observed.state, CodexRateLimitObservationState::Observed);
    assert_eq!(observed.bucket_count, 2);
    assert_eq!(observed.window_count, 3);
    assert_eq!(
        observed.legacy_map_identity_relation,
        orbit::provider_status::CodexLimitIdentityRelation::ExactMismatch
    );

    let snapshot = parse(result);
    assert_eq!(snapshot.state, AvailabilityState::Ready);
    assert_eq!(snapshot.quota_buckets.len(), 2);
    assert_eq!(snapshot.quota_windows.len(), 3);
    assert_eq!(snapshot.quota_buckets[0].windows.len(), 1);
    assert_eq!(snapshot.quota_buckets[1].windows.len(), 2);
    assert_eq!(
        snapshot.quota_buckets[0].provider_label.as_deref(),
        Some("synthetic-name-a")
    );
    assert_eq!(
        snapshot.quota_buckets[0].windows[0].remaining_percent,
        Some(75.0)
    );
    assert_eq!(
        snapshot.quota_buckets[0].windows[0].remaining_fraction,
        Some(0.75)
    );
    assert_eq!(
        snapshot.quota_buckets[1].windows[0].remaining_percent,
        Some(0.0)
    );
    assert_eq!(
        snapshot.quota_buckets[1].windows[1].remaining_percent,
        Some(100.0)
    );
    let durable = serde_json::to_string(&snapshot).unwrap();
    for private_value in [
        "synthetic-account",
        "synthetic-limit-a",
        "synthetic-limit-b",
        "synthetic-balance",
        "synthetic-credit-id",
        "synthetic-model-label",
    ] {
        assert!(!durable.contains(private_value));
    }
}

#[test]
fn codex_missing_mismatched_or_malformed_identity_stays_unknown() {
    let ordinary = json!({
        "accountId":"different-account",
        "ordinaryUsageAllowed":true,
        "rateLimits":{"primary":{"usedPercent":5,"resetsAt":null}}
    });
    let mismatch = parse(ordinary.clone());
    assert_eq!(mismatch.state, AvailabilityState::Unknown);
    assert_eq!(mismatch.quota_windows.len(), 1);
    assert_eq!(
        mismatch
            .provider_status_observation
            .as_ref()
            .unwrap()
            .quota_promotion,
        QuotaEvidencePromotion::ObservedUnconfirmed
    );
    let mut missing = ordinary;
    missing.as_object_mut().unwrap().remove("accountId");
    assert_eq!(parse(missing).state, AvailabilityState::Unknown);
    for malformed in [json!(null), json!(42), json!(""), json!("x".repeat(257))] {
        let mut status = json!({
            "ordinaryUsageAllowed":true,
            "rateLimits":{"primary":{"usedPercent":5}}
        });
        status["accountId"] = malformed;
        assert_eq!(parse(status).state, AvailabilityState::Unknown);
    }
    let invalid =
        codex_rate_limits_snapshot(&resource(), "fixture-account-id", b"not json", 10, 20).unwrap();
    assert_eq!(invalid.state, AvailabilityState::Unknown);
    assert!(invalid.quota_windows.is_empty());
    let oversized = codex_rate_limits_snapshot(
        &resource(),
        "fixture-account-id",
        &vec![b'x'; 65_537],
        10,
        20,
    )
    .unwrap();
    assert_eq!(oversized.state, AvailabilityState::Unknown);
}

#[test]
fn codex_ambiguous_shapes_and_version_mismatch_fail_closed() {
    let base = json!({
        "accountId":"fixture-account-id",
        "ordinaryUsageAllowed":true,
        "rateLimits":{"primary":{"usedPercent":30,"windowDurationMins":15,"resetsAt":null}}
    });
    let mut malformed = base.clone();
    malformed["rateLimits"]["primary"]["resetsAt"] = json!("tomorrow");
    assert_eq!(parse(malformed).state, AvailabilityState::Unknown);
    let mut missing_percent = base.clone();
    missing_percent["rateLimits"]["primary"]
        .as_object_mut()
        .unwrap()
        .remove("usedPercent");
    assert_eq!(parse(missing_percent).state, AvailabilityState::Unknown);
    let mut mismatched_bucket = base.clone();
    mismatched_bucket["rateLimitsByLimitId"] = json!({"codex":{"limitId":"other","primary":null}});
    assert_eq!(parse(mismatched_bucket).state, AvailabilityState::Unknown);
    let mut false_permission = base.clone();
    false_permission["ordinaryUsageAllowed"] = json!(false);
    assert_eq!(parse(false_permission).state, AvailabilityState::Limited);
    let mut future_shape = base.clone();
    future_shape["schemaVersion"] = json!(2);
    assert_eq!(parse(future_shape).state, AvailabilityState::Unknown);
    let mut wrong_runtime = resource();
    wrong_runtime.runtime.agent_revision = "0.153.4".into();
    assert!(
        codex_rate_limits_snapshot(
            &wrong_runtime,
            "fixture-account-id",
            &serde_json::to_vec(&base).unwrap(),
            10,
            20
        )
        .is_err()
    );
}

#[test]
fn normalized_execution_result_is_exact_and_never_invents_reset() {
    let r = resource();
    let exhausted = execution_result_snapshot(&r, TerminationReason::QuotaExhausted, 10, 20)
        .unwrap()
        .unwrap();
    assert_eq!(exhausted.state, AvailabilityState::QuotaExhausted);
    assert!(matches!(exhausted.applies_to, AvailabilityScope::Exact(_)));
    assert!(exhausted.quota_windows.is_empty());
    let limited = execution_result_snapshot(&r, TerminationReason::RateLimited, 10, 20)
        .unwrap()
        .unwrap();
    assert_eq!(limited.state, AvailabilityState::RateLimited);
    assert!(
        execution_result_snapshot(&r, TerminationReason::ResourceExhausted, 10, 20)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        effective_at(&r, &[exhausted], 21).unwrap().state,
        AvailabilityState::Unknown
    );
}
