//! Offline normalization for version-pinned provider status evidence.
//!
//! A caller must establish the isolated auth/account binding before invoking a
//! runtime status read. This module performs no I/O, repository access, model
//! dispatch or database coordination and never retains the raw response.
use crate::agy_usage_schema::{
    CompactSchemaSummary, GroupFieldShapeSummary, QuotaShapeSummary, SchemaCapture,
    compact_summary, describe_value, scrub_json_values,
};
use crate::availability::{
    AvailabilityScope, AvailabilitySnapshot, AvailabilityState, EvidenceConfidence, EvidenceSource,
    ExecutionResourceIdentity, ProviderQuotaGroup, ProviderQuotaGroupIdentityBasis,
    ProviderQuotaMember, QuotaBucket, QuotaBucketWindow, QuotaWindow,
};
use crate::continuation::TerminationReason;
use crate::model::digest;
use anyhow::{Result, ensure};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const CODEX_STATUS_REVISION: &str = "codex-app-server-0.156.0";
const MAX_STATUS_BYTES: usize = 65_536;
const MAX_BUCKETS: usize = 16;
const ANTIGRAVITY_USAGE_REVISION: &str = "antigravity-agy-1.2.9";
const MAX_AGY_GROUPS: usize = 16;
const MAX_AGY_BUCKETS_PER_GROUP: usize = 16;
const MAX_AGY_TOTAL_BUCKETS: usize = 16;
const MAX_AGY_WINDOW_BYTES: usize = 64;
const MAX_AGY_GROUP_LABEL_BYTES: usize = 128;
const MAX_AGY_GROUP_DESCRIPTION_BYTES: usize = 512;
const MAX_AGY_MEMBERS: usize = 32;
const MAX_EXTRACTION_DIAGNOSTICS: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ExtractionDiagnostic {
    pub path: String,
    pub field: String,
    pub value_type: &'static str,
    pub reason: &'static str,
    pub value: &'static str,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct AntigravityUsageCapture {
    pub schema: SchemaCapture,
    pub summary: CompactSchemaSummary,
    pub quota_buckets: Vec<QuotaBucket>,
    pub quota_groups: Vec<ProviderQuotaGroup>,
    pub extraction_diagnostics: Vec<ExtractionDiagnostic>,
    pub normalization_ready: bool,
    pub group_metadata_ready: bool,
    pub membership_ready: bool,
}

impl AntigravityUsageCapture {
    /// Build credential-scoped quota evidence only when the complete observed
    /// bucket structure passed validation. The status itself stays UNKNOWN:
    /// quota fractions alone do not define provider readiness.
    pub fn availability_snapshot(
        &self,
        credential: crate::availability::CredentialIdentity,
        observed_at_ms: i64,
        expires_at_ms: i64,
    ) -> Result<AvailabilitySnapshot> {
        ensure!(
            self.normalization_ready,
            "Antigravity quota normalization is blocked"
        );
        credential.validate()?;
        ensure!(
            credential.provider == "antigravity",
            "Antigravity status requires an Antigravity credential"
        );
        let evidence_digest = format!(
            "sha256:{}",
            digest(&serde_json::to_vec(&(
                "orbit.antigravity_usage_evidence.v1",
                &credential,
                &self.quota_buckets,
                &self.quota_groups,
                observed_at_ms,
                expires_at_ms
            ))?)
        );
        let snapshot = AvailabilitySnapshot {
            applies_to: AvailabilityScope::Credential(credential),
            observed_at_ms,
            expires_at_ms,
            state: AvailabilityState::Unknown,
            quota_windows: Vec::new(),
            quota_buckets: self.quota_buckets.clone(),
            quota_groups: self.quota_groups.clone(),
            source: EvidenceSource::ProviderNativeStatus,
            confidence: EvidenceConfidence::AuthoritativeNative,
            source_revision: ANTIGRAVITY_USAGE_REVISION.into(),
            evidence_digest,
            provider_observed_at_ms: None,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }
}

/// Parse an Antigravity `agy --print "/usage" --output-format json` document
/// into value-free diagnostics, explicitly approved group metadata, and quota
/// observations. Only exact group/member and quota-bucket contexts permit
/// values to leave the ephemeral JSON tree; all other values are discarded.
pub fn antigravity_usage_capture(raw: &[u8]) -> Result<AntigravityUsageCapture> {
    ensure!(
        raw.len() <= crate::agy_usage_schema::MAX_USAGE_JSON_BYTES,
        "Antigravity usage JSON exceeds its input bound"
    );
    let mut value: Value = serde_json::from_slice(raw)
        .map_err(|_| anyhow::anyhow!("Antigravity usage response is malformed JSON"))?;
    let result = antigravity_usage_capture_value(&value);
    scrub_json_values(&mut value);
    result
}

fn antigravity_usage_capture_value(value: &Value) -> Result<AntigravityUsageCapture> {
    let schema = describe_value(value)?;
    let quota_shape = summarize_antigravity_quota_shape(value);
    let mut diagnostics = Vec::new();
    let extraction = extract_antigravity_buckets(value, &mut diagnostics);
    let (quota_buckets, quota_groups, normalization_ready, group_metadata_ready, membership_ready) =
        match extraction {
            Ok((buckets, groups, groups_ready, members_ready)) => {
                (buckets, groups, true, groups_ready, members_ready)
            }
            Err(diagnostic) => {
                push_diagnostic(&mut diagnostics, diagnostic)?;
                (Vec::new(), Vec::new(), false, false, false)
            }
        };
    let summary = compact_summary(&schema, quota_shape)?;
    Ok(AntigravityUsageCapture {
        schema,
        summary,
        quota_buckets,
        quota_groups,
        extraction_diagnostics: diagnostics,
        normalization_ready,
        group_metadata_ready,
        membership_ready,
    })
}

fn extract_antigravity_buckets(
    value: &Value,
    diagnostics: &mut Vec<ExtractionDiagnostic>,
) -> Result<(Vec<QuotaBucket>, Vec<ProviderQuotaGroup>, bool, bool), ExtractionDiagnostic> {
    let Some(command) = value.get("command").and_then(Value::as_object) else {
        return Err(diagnostic(
            "$.command",
            "command",
            value.get("command"),
            "expected-object",
        ));
    };
    let Some(data) = command.get("data").and_then(Value::as_object) else {
        return Err(diagnostic(
            "$.command.data",
            "data",
            command.get("data"),
            "expected-object",
        ));
    };
    let Some(groups) = data.get("groups").and_then(Value::as_array) else {
        return Err(diagnostic(
            "$.command.data.groups",
            "groups",
            data.get("groups"),
            "expected-array",
        ));
    };
    if groups.len() > MAX_AGY_GROUPS {
        return Err(diagnostic(
            "$.command.data.groups",
            "groups",
            data.get("groups"),
            "group-count-exceeds-bound",
        ));
    }

    let mut buckets_by_fingerprint = BTreeMap::<String, Vec<QuotaBucketWindow>>::new();
    let mut bucket_group = BTreeMap::<String, usize>::new();
    let mut quota_groups = Vec::with_capacity(groups.len());
    let mut group_metadata_ready = !groups.is_empty();
    let mut membership_ready = !groups.is_empty();
    for (group_index, group) in groups.iter().enumerate() {
        let group_path = format!("$.command.data.groups[{group_index}]");
        let Some(group_object) = group.as_object() else {
            return Err(diagnostic(
                &group_path,
                "group",
                Some(group),
                "expected-object",
            ));
        };
        let Some(buckets) = group_object.get("buckets").and_then(Value::as_array) else {
            return Err(diagnostic(
                &format!("{group_path}.buckets"),
                "buckets",
                group_object.get("buckets"),
                "expected-array",
            ));
        };
        if buckets.len() > MAX_AGY_BUCKETS_PER_GROUP
            || buckets_by_fingerprint.len() + buckets.len() > MAX_AGY_TOTAL_BUCKETS
        {
            return Err(diagnostic(
                &format!("{group_path}.buckets"),
                "buckets",
                Some(&Value::Array(buckets.clone())),
                "bucket-count-exceeds-bound",
            ));
        }
        let mut group_bucket_fingerprints = BTreeSet::new();
        for (bucket_index, bucket) in buckets.iter().enumerate() {
            let bucket_path = format!("{group_path}.buckets[{bucket_index}]");
            let Some(bucket_object) = bucket.as_object() else {
                return Err(diagnostic(
                    &bucket_path,
                    "bucket",
                    Some(bucket),
                    "expected-object",
                ));
            };
            // This exact nested `id` is the provider's quota-resource identity,
            // not an account identity. It is validated and fingerprinted in
            // memory only; the raw value never enters a returned structure.
            let Some(bucket_id) = bucket_object.get("id").and_then(Value::as_str) else {
                return Err(diagnostic(
                    &format!("{bucket_path}.id"),
                    "id",
                    bucket_object.get("id"),
                    "expected-quota-bucket-identifier",
                ));
            };
            if bucket_id.is_empty()
                || bucket_id.len() > 256
                || bucket_id.chars().any(char::is_control)
            {
                return Err(diagnostic(
                    &format!("{bucket_path}.id"),
                    "id",
                    bucket_object.get("id"),
                    "invalid-quota-bucket-identifier-shape",
                ));
            }
            if bucket_group
                .get(bucket_id)
                .is_some_and(|first_group| *first_group != group_index)
            {
                return Err(diagnostic(
                    &format!("{bucket_path}.id"),
                    "id",
                    bucket_object.get("id"),
                    "quota-bucket-identity-collides-across-groups",
                ));
            }
            bucket_group.insert(bucket_id.to_owned(), group_index);
            let fingerprint = format!(
                "qb1:{}",
                digest(
                    &serde_json::to_vec(&(
                        "orbit.provider_quota_bucket.v1",
                        "antigravity",
                        bucket_id
                    ))
                    .map_err(|_| {
                        diagnostic(
                            &format!("{bucket_path}.id"),
                            "id",
                            bucket_object.get("id"),
                            "quota-fingerprint-failed",
                        )
                    })?
                )
            );
            group_bucket_fingerprints.insert(fingerprint.clone());
            buckets_by_fingerprint
                .entry(fingerprint.clone())
                .or_default();

            let remaining_fraction = match bucket_object.get("remaining_fraction") {
                None | Some(Value::Null) => None,
                Some(Value::Number(number)) => match number.as_f64() {
                    Some(fraction) if fraction.is_finite() => Some(fraction),
                    _ => {
                        return Err(diagnostic(
                            &format!("{bucket_path}.remaining_fraction"),
                            "remaining_fraction",
                            bucket_object.get("remaining_fraction"),
                            "fraction-is-not-finite-number",
                        ));
                    }
                },
                Some(_) => {
                    return Err(diagnostic(
                        &format!("{bucket_path}.remaining_fraction"),
                        "remaining_fraction",
                        bucket_object.get("remaining_fraction"),
                        "fraction-has-invalid-type",
                    ));
                }
            };

            let provider_reset_time = match bucket_object.get("reset_time") {
                None | Some(Value::Null) => None,
                Some(Value::String(text)) => match parse_agy_utc_timestamp(text) {
                    Some((timestamp, at_ms)) => Some((timestamp.to_owned(), at_ms)),
                    None => {
                        push_diagnostic(
                            diagnostics,
                            diagnostic(
                                &format!("{bucket_path}.reset_time"),
                                "reset_time",
                                bucket_object.get("reset_time"),
                                "timestamp-format-or-value-unsupported",
                            ),
                        )
                        .map_err(|_| {
                            diagnostic(
                                &format!("{bucket_path}.reset_time"),
                                "reset_time",
                                bucket_object.get("reset_time"),
                                "diagnostic-bound-exceeded",
                            )
                        })?;
                        None
                    }
                },
                Some(_) => {
                    push_diagnostic(
                        diagnostics,
                        diagnostic(
                            &format!("{bucket_path}.reset_time"),
                            "reset_time",
                            bucket_object.get("reset_time"),
                            "timestamp-has-invalid-type",
                        ),
                    )
                    .map_err(|_| {
                        diagnostic(
                            &format!("{bucket_path}.reset_time"),
                            "reset_time",
                            bucket_object.get("reset_time"),
                            "diagnostic-bound-exceeded",
                        )
                    })?;
                    None
                }
            };

            let provider_window = match bucket_object.get("window") {
                None | Some(Value::Null) => None,
                Some(Value::String(window))
                    if !window.is_empty()
                        && window.len() <= MAX_AGY_WINDOW_BYTES
                        && !window.chars().any(char::is_control) =>
                {
                    Some(window.clone())
                }
                Some(_) => {
                    return Err(diagnostic(
                        &format!("{bucket_path}.window"),
                        "window",
                        bucket_object.get("window"),
                        "window-has-invalid-type-or-shape",
                    ));
                }
            };
            let Some(provider_window_id) = provider_window else {
                if remaining_fraction.is_some() || provider_reset_time.is_some() {
                    return Err(diagnostic(
                        &format!("{bucket_path}.window"),
                        "window",
                        bucket_object.get("window"),
                        "window-identity-missing-for-quota-values",
                    ));
                }
                continue;
            };

            let (provider_reset_time, resets_at_ms) = provider_reset_time
                .map(|(timestamp, at_ms)| (Some(timestamp), Some(at_ms)))
                .unwrap_or((None, None));
            let windows = buckets_by_fingerprint
                .get_mut(&fingerprint)
                .expect("quota bucket entry inserted above");
            if windows
                .iter()
                .any(|window| window.provider_window_id == provider_window_id)
            {
                return Err(diagnostic(
                    &format!("{bucket_path}.window"),
                    "window",
                    bucket_object.get("window"),
                    "duplicate-window-for-quota-bucket",
                ));
            }
            windows.push(QuotaBucketWindow {
                provider_window_id,
                duration_minutes: None,
                used_percent: None,
                remaining_percent: None,
                remaining_fraction,
                resets_at_ms,
                provider_reset_time,
                exhausted: None,
            });
        }

        let (metadata, group_membership_ready) = extract_group_metadata(
            group_object,
            &group_path,
            group_bucket_fingerprints,
            diagnostics,
        )?;
        membership_ready &= group_membership_ready;
        match metadata {
            Some(metadata) => quota_groups.push(metadata),
            None => {
                group_metadata_ready = false;
                membership_ready = false;
            }
        }
    }

    let buckets = buckets_by_fingerprint
        .into_iter()
        .map(|(provider_bucket_fingerprint, mut windows)| {
            windows.sort_by(|left, right| left.provider_window_id.cmp(&right.provider_window_id));
            QuotaBucket {
                provider_bucket_fingerprint,
                scope: None,
                windows,
            }
        })
        .collect();
    quota_groups.sort_by(|left, right| left.fingerprint.cmp(&right.fingerprint));
    let mut group_fingerprints = BTreeSet::new();
    for group in &quota_groups {
        if !group_fingerprints.insert(&group.fingerprint) {
            record_diagnostic(
                diagnostics,
                "$.command.data.groups",
                "group-identity",
                None,
                "quota-group-identity-collision",
            )?;
            group_metadata_ready = false;
            membership_ready = false;
            break;
        }
    }
    if quota_groups.len() != groups.len() {
        group_metadata_ready = false;
        membership_ready = false;
    }
    Ok((
        buckets,
        quota_groups,
        group_metadata_ready,
        membership_ready,
    ))
}

fn summarize_antigravity_quota_shape(value: &Value) -> Option<QuotaShapeSummary> {
    let groups = value
        .get("command")?
        .get("data")?
        .get("groups")?
        .as_array()?;
    let mut buckets_per_group = Vec::with_capacity(groups.len());
    let mut observed_fields = BTreeSet::new();
    let mut group_fields = BTreeMap::<
        String,
        (
            BTreeSet<&'static str>,
            BTreeSet<&'static str>,
            BTreeSet<String>,
        ),
    >::new();
    let expected_fields = ["id", "name", "remaining_fraction", "reset_time", "window"];
    let mut additional_bucket_field_count = 0usize;
    for group in groups {
        let group_object = group.as_object()?;
        for (field, field_value) in group_object {
            let entry = group_fields.entry(field.clone()).or_default();
            entry.0.insert(json_value_type(field_value));
            if let Some(items) = field_value.as_array() {
                for item in items {
                    entry.1.insert(json_value_type(item));
                    if let Some(object) = item.as_object() {
                        entry.2.extend(object.keys().cloned());
                    }
                }
            }
        }
        let buckets = group.get("buckets")?.as_array()?;
        buckets_per_group.push(buckets.len());
        for bucket in buckets {
            let object = bucket.as_object()?;
            for field in object.keys() {
                observed_fields.insert(field.clone());
                if !expected_fields.contains(&field.as_str()) {
                    additional_bucket_field_count += 1;
                }
            }
        }
    }
    Some(QuotaShapeSummary {
        groups: groups.len(),
        buckets_per_group,
        observed_group_fields: group_fields
            .into_iter()
            .map(
                |(field, (value_types, array_element_types, array_object_fields))| {
                    GroupFieldShapeSummary {
                        field,
                        value_types: value_types.into_iter().collect(),
                        array_element_types: array_element_types.into_iter().collect(),
                        array_object_fields: array_object_fields.into_iter().collect(),
                    }
                },
            )
            .collect(),
        observed_bucket_fields: observed_fields.into_iter().collect(),
        additional_bucket_field_count,
    })
}

const GROUP_DESCRIPTION_PREFIX: &str = "Models within this group:";

/// Extract only provider `name` and `description` directly from a quota group.
/// A malformed description affects member evidence only; valid names and the
/// structurally parented bucket fingerprints remain usable.
fn extract_group_metadata(
    group: &serde_json::Map<String, Value>,
    path: &str,
    bucket_fingerprints: BTreeSet<String>,
    diagnostics: &mut Vec<ExtractionDiagnostic>,
) -> Result<(Option<ProviderQuotaGroup>, bool), ExtractionDiagnostic> {
    let provider_display_name = match group.get("name") {
        Some(Value::String(value))
            if safe_provider_group_text(value, MAX_AGY_GROUP_LABEL_BYTES) =>
        {
            Some(value.clone())
        }
        Some(value) => {
            record_diagnostic(
                diagnostics,
                &format!("{path}.name"),
                "name",
                Some(value),
                "group-name-has-invalid-type-or-shape",
            )?;
            None
        }
        None => {
            record_diagnostic(
                diagnostics,
                &format!("{path}.name"),
                "name",
                None,
                "group-name-field-missing",
            )?;
            None
        }
    };
    let Some(provider_display_name) = provider_display_name else {
        return Ok((None, false));
    };

    if bucket_fingerprints.is_empty() {
        record_diagnostic(
            diagnostics,
            &format!("{path}.buckets"),
            "buckets",
            group.get("buckets"),
            "group-has-no-quota-buckets",
        )?;
        return Ok((None, false));
    }

    let description = group.get("description");
    let (provider_description, members, membership_ready) = match description {
        Some(Value::String(value))
            if safe_provider_group_text(value, MAX_AGY_GROUP_DESCRIPTION_BYTES) =>
        {
            match parse_group_members(value) {
                Ok(members) => (Some(value.clone()), members, true),
                Err(reason) => {
                    record_diagnostic(
                        diagnostics,
                        &format!("{path}.description"),
                        "description",
                        description,
                        reason,
                    )?;
                    (Some(value.clone()), Vec::new(), false)
                }
            }
        }
        Some(Value::Null) => {
            record_diagnostic(
                diagnostics,
                &format!("{path}.description"),
                "description",
                description,
                "group-description-null",
            )?;
            (None, Vec::new(), false)
        }
        Some(value) => {
            record_diagnostic(
                diagnostics,
                &format!("{path}.description"),
                "description",
                Some(value),
                "group-description-has-invalid-type-or-shape",
            )?;
            (None, Vec::new(), false)
        }
        None => {
            record_diagnostic(
                diagnostics,
                &format!("{path}.description"),
                "description",
                None,
                "group-description-field-missing",
            )?;
            (None, Vec::new(), false)
        }
    };

    let canonical_name = canonical_group_component(&provider_display_name);
    let mut canonical_members = members
        .iter()
        .map(|member| canonical_group_component(&member.provider_label))
        .collect::<Vec<_>>();
    canonical_members.sort();
    let identity_material = serde_json::to_vec(&(
        "orbit.antigravity_quota_group.v1",
        "antigravity",
        canonical_name,
        canonical_members,
    ))
    .map_err(|_| diagnostic(path, "group-identity", None, "group-fingerprint-failed"))?;
    let fingerprint = format!("qg1:{}", digest(&identity_material));
    let mut bucket_fingerprints = bucket_fingerprints.into_iter().collect::<Vec<_>>();
    bucket_fingerprints.sort();
    Ok((
        Some(ProviderQuotaGroup {
            fingerprint,
            identity_basis: ProviderQuotaGroupIdentityBasis::DisplayNameAndMemberSet,
            provider_display_name: Some(provider_display_name),
            provider_description,
            members,
            bucket_fingerprints,
        }),
        membership_ready,
    ))
}

fn parse_group_members(
    description: &str,
) -> std::result::Result<Vec<ProviderQuotaMember>, &'static str> {
    let Some(member_text) = description.strip_prefix(GROUP_DESCRIPTION_PREFIX) else {
        return Err("group-description-membership-prefix-unsupported");
    };
    let member_text = member_text.trim();
    if member_text.is_empty() {
        return Err("group-description-member-list-empty");
    }
    let labels = member_text.split(',').map(str::trim).collect::<Vec<_>>();
    if labels.len() > MAX_AGY_MEMBERS {
        return Err("group-description-member-count-exceeds-bound");
    }
    let mut canonical_labels = BTreeSet::new();
    let mut members = Vec::with_capacity(labels.len());
    for label in labels {
        if label.is_empty() {
            return Err("group-description-contains-empty-member");
        }
        if !safe_provider_group_text(label, MAX_AGY_GROUP_LABEL_BYTES) {
            return Err("group-description-member-has-invalid-type-or-shape");
        }
        if !canonical_labels.insert(canonical_group_component(label)) {
            return Err("group-description-contains-duplicate-member");
        }
        members.push(ProviderQuotaMember {
            provider_label: label.to_owned(),
            provider_key_fingerprint: None,
        });
    }
    members.sort_by(|left, right| left.provider_label.cmp(&right.provider_label));
    Ok(members)
}

fn canonical_group_component(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn safe_provider_group_text(value: &str, max_bytes: usize) -> bool {
    !value.trim().is_empty()
        && value.len() <= max_bytes
        && !value.chars().any(char::is_control)
        && !value.contains('@')
        && !value.contains("://")
        && value.split_whitespace().all(|word| word.len() <= 40)
        && (value.chars().any(char::is_whitespace) || value.len() <= 40)
}

fn record_diagnostic(
    diagnostics: &mut Vec<ExtractionDiagnostic>,
    path: &str,
    field: &str,
    value: Option<&Value>,
    reason: &'static str,
) -> Result<(), ExtractionDiagnostic> {
    push_diagnostic(diagnostics, diagnostic(path, field, value, reason))
        .map_err(|_| diagnostic(path, field, value, "diagnostic-bound-exceeded"))
}

fn diagnostic(
    path: &str,
    field: &str,
    value: Option<&Value>,
    reason: &'static str,
) -> ExtractionDiagnostic {
    ExtractionDiagnostic {
        path: path.to_owned(),
        field: field.to_owned(),
        value_type: value.map_or("missing", json_value_type),
        reason,
        value: "<REDACTED>",
    }
}

fn push_diagnostic(
    diagnostics: &mut Vec<ExtractionDiagnostic>,
    diagnostic: ExtractionDiagnostic,
) -> Result<()> {
    ensure!(
        diagnostics.len() < MAX_EXTRACTION_DIAGNOSTICS,
        "Antigravity usage extraction diagnostics exceed their bound"
    );
    diagnostics.push(diagnostic);
    Ok(())
}

fn json_value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Accept only the compact UTC form observed structurally for this adapter.
/// No alternate timezone, fractional-second or provider-specific format is
/// guessed. Returns the exact approved source string and epoch milliseconds.
fn parse_agy_utc_timestamp(value: &str) -> Option<(&str, i64)> {
    let bytes = value.as_bytes();
    if bytes.len() != 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
    {
        return None;
    }
    let year = digits(bytes, 0, 4)?;
    let month = digits(bytes, 5, 2)?;
    let day = digits(bytes, 8, 2)?;
    let hour = digits(bytes, 11, 2)?;
    let minute = digits(bytes, 14, 2)?;
    let second = digits(bytes, 17, 2)?;
    if !(1970..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=59).contains(&second)
    {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if day < 1 || day > days_in_month {
        return None;
    }
    let days = days_from_civil(year, month, day)?;
    let seconds = days
        .checked_mul(86_400)?
        .checked_add(i64::from(hour) * 3_600)?
        .checked_add(i64::from(minute) * 60)?
        .checked_add(i64::from(second))?;
    Some((value, seconds.checked_mul(1_000)?))
}

fn digits(bytes: &[u8], start: usize, count: usize) -> Option<i32> {
    let mut number = 0i32;
    for byte in bytes.get(start..start + count)? {
        if !byte.is_ascii_digit() {
            return None;
        }
        number = number
            .checked_mul(10)?
            .checked_add(i32::from(byte - b'0'))?;
    }
    Some(number)
}

fn days_from_civil(year: i32, month: i32, day: i32) -> Option<i64> {
    let adjusted_year = year - i32::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    i64::from(era)
        .checked_mul(146_097)?
        .checked_add(i64::from(day_of_era))?
        .checked_sub(719_468)
        .filter(|days| *days >= 0)
}

/// Normalize only the `result` of pinned Codex App Server
/// `account/rateLimits/read`. The caller must establish either independent
/// expected-ID equality or equality of a provider-scope fingerprint against a
/// previously confirmed durable credential-generation binding. Only then may
/// it pass the observed raw ID as `expected_account_id`; an unconfirmed first
/// observation must pass no expected ID. Missing or mismatched identity yields
/// UNKNOWN and discards all quota facts.
pub fn codex_rate_limits_snapshot(
    resource: &ExecutionResourceIdentity,
    expected_account_id: &str,
    result_bytes: &[u8],
    observed_at_ms: i64,
    expires_at_ms: i64,
) -> Result<AvailabilitySnapshot> {
    resource.validate()?;
    ensure!(
        resource.runtime.agent_revision == crate::codex_bridge::CODEX_VERSION
            && resource.runtime.adapter == "codex_bridge",
        "unsupported Codex status adapter revision"
    );
    let mut snapshot = AvailabilitySnapshot {
        applies_to: AvailabilityScope::Credential(resource.credential.clone()),
        observed_at_ms,
        expires_at_ms,
        state: AvailabilityState::Unknown,
        quota_windows: Vec::new(),
        quota_buckets: Vec::new(),
        quota_groups: Vec::new(),
        source: EvidenceSource::RuntimeNativeStatus,
        confidence: EvidenceConfidence::Unknown,
        source_revision: CODEX_STATUS_REVISION.into(),
        evidence_digest: format!(
            "sha256:{}",
            if result_bytes.len() <= MAX_STATUS_BYTES {
                digest(result_bytes)
            } else {
                digest(b"orbit.codex_status.oversized.v1")
            }
        ),
        provider_observed_at_ms: None,
    };
    snapshot.validate()?;
    if result_bytes.len() > MAX_STATUS_BYTES
        || expected_account_id.is_empty()
        || expected_account_id.len() > 256
    {
        return Ok(snapshot);
    }
    let Ok(value) = serde_json::from_slice::<Value>(result_bytes) else {
        return Ok(snapshot);
    };
    let Some(body) = value.as_object() else {
        return Ok(snapshot);
    };
    // The pinned result type has no version discriminator. A newly versioned
    // response cannot inherit this adapter's interpretation implicitly.
    if body.contains_key("version") || body.contains_key("schemaVersion") {
        return Ok(snapshot);
    }
    let Some(account_id) = body.get("accountId").and_then(Value::as_str) else {
        return Ok(snapshot);
    };
    if account_id.is_empty() || account_id.len() > 256 || account_id != expected_account_id {
        return Ok(snapshot);
    }
    let allowed = match body.get("ordinaryUsageAllowed") {
        Some(Value::Bool(value)) => Some(*value),
        Some(Value::Null) | None => None,
        _ => return Ok(snapshot),
    };
    let Some(default) = body.get("rateLimits").and_then(Value::as_object) else {
        return Ok(snapshot);
    };
    let mut windows = Vec::new();
    let mut quota_buckets = Vec::new();
    match body.get("rateLimitsByLimitId") {
        Some(Value::Object(buckets)) if !buckets.is_empty() => {
            if buckets.len() > MAX_BUCKETS {
                return Ok(snapshot);
            }
            for (key, bucket) in buckets {
                let Some(bucket) = bucket.as_object() else {
                    return Ok(snapshot);
                };
                if bucket.get("limitId").and_then(Value::as_str) != Some(key.as_str()) {
                    return Ok(snapshot);
                }
                if key.is_empty() || key.len() > 256 || key.chars().any(char::is_control) {
                    return Ok(snapshot);
                }
                let label = format!("bucket.{}", digest(key.as_bytes()));
                let Some(parsed) = parse_windows(bucket) else {
                    return Ok(snapshot);
                };
                windows.extend(project_legacy_windows(&parsed, &label));
                quota_buckets.push(QuotaBucket {
                    provider_bucket_fingerprint: format!(
                        "qb1:{}",
                        digest(&serde_json::to_vec(&(
                            "orbit.provider_quota_bucket.v1",
                            resource.credential.provider.as_str(),
                            key.as_str()
                        ))?)
                    ),
                    scope: None,
                    windows: parsed,
                });
            }
        }
        Some(Value::Null) | None | Some(Value::Object(_)) => {
            let Some(parsed) = parse_windows(default) else {
                return Ok(snapshot);
            };
            windows = project_legacy_windows(&parsed, "default");
        }
        _ => return Ok(snapshot),
    }
    snapshot.confidence = EvidenceConfidence::AuthoritativeNative;
    // Account-wide permission is not model-specific availability. Even READY
    // here cannot establish READY for an exact candidate in effective_at.
    snapshot.state = match allowed {
        Some(true) => AvailabilityState::Ready,
        Some(false) => AvailabilityState::Limited,
        None => AvailabilityState::Unknown,
    };
    snapshot.quota_windows = windows;
    snapshot.quota_buckets = quota_buckets;
    snapshot.validate()?;
    Ok(snapshot)
}

fn parse_windows(bucket: &serde_json::Map<String, Value>) -> Option<Vec<QuotaBucketWindow>> {
    let mut windows = Vec::new();
    for name in ["primary", "secondary"] {
        let Some(value) = bucket.get(name) else {
            continue;
        };
        if value.is_null() {
            continue;
        }
        let object = value.as_object()?;
        let used_percent = object.get("usedPercent")?.as_f64()?;
        if !used_percent.is_finite() || !(0.0..=100.0).contains(&used_percent) {
            return None;
        }
        let duration_minutes = optional_positive_i64(object.get("windowDurationMins"))?;
        let resets_at_ms = match optional_positive_i64(object.get("resetsAt"))? {
            Some(seconds) => Some(seconds.checked_mul(1000)?),
            None => None,
        };
        windows.push(QuotaBucketWindow {
            provider_window_id: name.to_owned(),
            duration_minutes,
            used_percent: Some(used_percent),
            remaining_percent: None,
            remaining_fraction: None,
            resets_at_ms,
            provider_reset_time: None,
            exhausted: None,
        });
    }
    Some(windows)
}

fn project_legacy_windows(windows: &[QuotaBucketWindow], label: &str) -> Vec<QuotaWindow> {
    windows
        .iter()
        .map(|window| QuotaWindow {
            label: format!("{label}.{}", window.provider_window_id),
            duration_minutes: window.duration_minutes,
            used_percent: window.used_percent,
            remaining_percent: window.remaining_percent,
            resets_at_ms: window.resets_at_ms,
            exhausted: window.exhausted,
        })
        .collect()
}

fn optional_positive_i64(value: Option<&Value>) -> Option<Option<i64>> {
    match value {
        None | Some(Value::Null) => Some(None),
        Some(Value::Number(value)) => value.as_i64().filter(|v| *v > 0).map(Some),
        _ => None,
    }
}

/// Convert a *confirmed normalized* execution result into exact-resource
/// negative evidence. No provider reset or quota window is invented.
/// Ambiguous RESOURCE_EXHAUSTED and all other results produce no snapshot.
pub fn execution_result_snapshot(
    resource: &ExecutionResourceIdentity,
    reason: TerminationReason,
    observed_at_ms: i64,
    expires_at_ms: i64,
) -> Result<Option<AvailabilitySnapshot>> {
    resource.validate()?;
    let state = match reason {
        TerminationReason::QuotaExhausted => AvailabilityState::QuotaExhausted,
        TerminationReason::RateLimited => AvailabilityState::RateLimited,
        _ => return Ok(None),
    };
    let snapshot = AvailabilitySnapshot {
        applies_to: AvailabilityScope::Exact(resource.clone()),
        observed_at_ms,
        expires_at_ms,
        state,
        quota_windows: Vec::new(),
        quota_buckets: Vec::new(),
        quota_groups: Vec::new(),
        source: EvidenceSource::ExecutionResult,
        confidence: EvidenceConfidence::ExecutionObserved,
        source_revision: "normalized-agent-result-v1".into(),
        evidence_digest: format!(
            "sha256:{}",
            digest(&serde_json::to_vec(&(
                "orbit.execution_result_status.v1",
                resource.id()?,
                reason,
                observed_at_ms
            ))?)
        ),
        provider_observed_at_ms: None,
    };
    snapshot.validate()?;
    Ok(Some(snapshot))
}
