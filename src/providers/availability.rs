//! Offline, provider-neutral resource identity and availability evidence.
//!
//! This module does not probe providers or authorize a claim. PostgreSQL owns
//! the current evidence; a future scheduler must still enforce policy, worker
//! capacity, credential health and resource leases before dispatch.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Row, Transaction};

fn logical_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:@+-".contains(&b))
}

fn opaque_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

fn sha256_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn versioned_sha256(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeIdentity {
    pub family: String,
    pub adapter: String,
    pub binding: String,
    pub image_digest: String,
    pub agent_revision: String,
    pub adapter_version: String,
}

impl RuntimeIdentity {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            [
                &self.family,
                &self.adapter,
                &self.binding,
                &self.agent_revision,
                &self.adapter_version,
            ]
            .iter()
            .all(|v| logical_id(v)),
            "invalid logical runtime identity"
        );
        ensure!(
            sha256_digest(&self.image_digest),
            "runtime image must be pinned by SHA-256"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialIdentity {
    pub provider: String,
    pub reference: String,
    pub generation: String,
    /// Present only for registry-owned credentials. Legacy logical identities
    /// serialize exactly as before and cannot acquire catalog evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_id: Option<String>,
}

impl CredentialIdentity {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            [&self.provider, &self.reference, &self.generation]
                .iter()
                .all(|v| logical_id(v))
                && self.catalog_id.as_ref().is_none_or(|id| {
                    uuid::Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == *id)
                }),
            "credential identity must contain only logical identifiers"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionResourceIdentity {
    pub runtime: RuntimeIdentity,
    pub credential: CredentialIdentity,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

impl ExecutionResourceIdentity {
    pub fn validate(&self) -> Result<()> {
        self.runtime.validate()?;
        self.credential.validate()?;
        ensure!(opaque_id(&self.model), "invalid model identity");
        ensure!(
            self.reasoning_effort.as_deref().is_none_or(opaque_id),
            "invalid reasoning effort identity"
        );
        Ok(())
    }

    /// Versioned and domain-separated; display names and secret values are absent.
    pub fn id(&self) -> Result<String> {
        self.validate()?;
        Ok(format!(
            "er1:{}",
            crate::model::digest(&serde_json::to_vec(&("orbit.execution_resource.v1", self))?)
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "identity", rename_all = "snake_case")]
pub enum AvailabilityScope {
    Exact(ExecutionResourceIdentity),
    CredentialModel {
        credential: CredentialIdentity,
        model: String,
    },
    Credential(CredentialIdentity),
    Runtime(RuntimeIdentity),
}

impl AvailabilityScope {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Exact(resource) => resource.validate(),
            Self::CredentialModel { credential, model } => {
                credential.validate()?;
                ensure!(opaque_id(model), "invalid scoped model identity");
                Ok(())
            }
            Self::Credential(credential) => credential.validate(),
            Self::Runtime(runtime) => runtime.validate(),
        }
    }

    pub fn key(&self) -> Result<String> {
        self.validate()?;
        if self.has_catalog_credential() {
            let mut stable = self.clone();
            stable.map_credential_identity(normalize_catalog_reference);
            return Ok(format!(
                "as2:{}",
                crate::model::digest(&serde_json::to_vec(&(
                    "orbit.availability_scope.catalog.v2",
                    stable
                ))?)
            ));
        }
        Ok(format!(
            "as1:{}",
            crate::model::digest(&serde_json::to_vec(&("orbit.availability_scope.v1", self))?)
        ))
    }

    fn has_catalog_credential(&self) -> bool {
        match self {
            Self::Exact(resource) => resource.credential.catalog_id.is_some(),
            Self::CredentialModel { credential, .. } | Self::Credential(credential) => {
                credential.catalog_id.is_some()
            }
            Self::Runtime(_) => false,
        }
    }

    fn map_credential_identity(&mut self, mut map: impl FnMut(&mut CredentialIdentity)) {
        match self {
            Self::Exact(resource) => map(&mut resource.credential),
            Self::CredentialModel { credential, .. } | Self::Credential(credential) => {
                map(credential)
            }
            Self::Runtime(_) => {}
        }
    }

    pub fn matches(&self, resource: &ExecutionResourceIdentity) -> bool {
        match self {
            Self::Exact(r) => {
                r.runtime == resource.runtime
                    && credential_identity_matches(&r.credential, &resource.credential)
                    && r.model == resource.model
                    && r.reasoning_effort == resource.reasoning_effort
            }
            Self::CredentialModel { credential, model } => {
                credential_identity_matches(credential, &resource.credential)
                    && model == &resource.model
            }
            Self::Credential(c) => credential_identity_matches(c, &resource.credential),
            Self::Runtime(r) => r == &resource.runtime,
        }
    }
}

fn normalize_catalog_reference(credential: &mut CredentialIdentity) {
    if let Some(catalog_id) = &credential.catalog_id {
        credential.reference = format!("catalog-{catalog_id}");
    }
}

fn credential_identity_matches(left: &CredentialIdentity, right: &CredentialIdentity) -> bool {
    match (&left.catalog_id, &right.catalog_id) {
        (Some(left_id), Some(right_id)) => {
            left_id == right_id
                && left.provider == right.provider
                && left.generation == right.generation
        }
        (None, None) => left == right,
        _ => false,
    }
}

fn stable_scope_key(scope: &AvailabilityScope) -> Result<String> {
    let mut stable = scope.clone();
    if stable.has_catalog_credential() {
        stable.map_credential_identity(normalize_catalog_reference);
    }
    stable.key()
}

fn legacy_scope_key(scope: &AvailabilityScope) -> Result<String> {
    scope.validate()?;
    Ok(format!(
        "as1:{}",
        crate::model::digest(&serde_json::to_vec(&(
            "orbit.availability_scope.v1",
            scope
        ))?)
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AvailabilityState {
    Ready,
    Limited,
    Cooldown,
    RateLimited,
    QuotaExhausted,
    AuthFailed,
    RuntimeUnavailable,
    CapabilityMismatch,
    Unknown,
}

impl AvailabilityState {
    fn blocking(self) -> bool {
        matches!(
            self,
            Self::Cooldown
                | Self::RateLimited
                | Self::QuotaExhausted
                | Self::AuthFailed
                | Self::RuntimeUnavailable
                | Self::CapabilityMismatch
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    ProviderNativeStatus,
    RuntimeNativeStatus,
    ExecutionResult,
    OperatorOverride,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceConfidence {
    AuthoritativeNative,
    ExecutionObserved,
    OperatorAsserted,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaWindow {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_minutes: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exhausted: Option<bool>,
}

impl QuotaWindow {
    fn validate(&self) -> Result<()> {
        ensure!(opaque_id(&self.label), "invalid quota window label");
        validate_quota_values(
            self.duration_minutes,
            self.used_percent,
            self.remaining_percent,
            self.resets_at_ms,
        )
    }
}

fn validate_quota_values(
    duration_minutes: Option<i64>,
    used_percent: Option<f64>,
    remaining_percent: Option<f64>,
    resets_at_ms: Option<i64>,
) -> Result<()> {
    ensure!(
        duration_minutes.is_none_or(|v| v > 0),
        "invalid quota window duration"
    );
    for value in [used_percent, remaining_percent].into_iter().flatten() {
        ensure!(
            value.is_finite() && (0.0..=100.0).contains(&value),
            "invalid quota percent"
        );
    }
    ensure!(
        resets_at_ms.is_none_or(|v| v >= 0),
        "invalid quota reset time"
    );
    Ok(())
}

/// A provider quota window nested under its provider bucket. `provider_window_id`
/// is the source's bounded opaque window discriminator (for example, primary),
/// not a duration-derived or human-friendly label.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaBucketWindow {
    pub provider_window_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_minutes: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_percent: Option<f64>,
    /// Provider's exact fraction when the provider reports a fraction directly.
    /// Adapters validate provider-specific range semantics before populating it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_fraction: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_ms: Option<i64>,
    /// Exact bounded provider timestamp, retained separately from Orbit's
    /// snapshot expiry/freshness timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_reset_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exhausted: Option<bool>,
}

impl QuotaBucketWindow {
    fn validate(&self) -> Result<()> {
        ensure!(
            opaque_id(&self.provider_window_id),
            "invalid provider quota window identity"
        );
        ensure!(
            self.remaining_fraction.is_none_or(f64::is_finite),
            "invalid provider quota fraction"
        );
        ensure!(
            self.provider_reset_time.as_deref().is_none_or(|value| {
                !value.is_empty() && value.len() <= 40 && !value.chars().any(char::is_control)
            }),
            "invalid provider quota reset timestamp"
        );
        validate_quota_values(
            self.duration_minutes,
            self.used_percent,
            self.remaining_percent,
            self.resets_at_ms,
        )
    }

    fn matches_legacy(&self, window: &QuotaWindow) -> bool {
        self.duration_minutes == window.duration_minutes
            && self.used_percent == window.used_percent
            && self.remaining_percent == window.remaining_percent
            && self.remaining_fraction.is_none_or(|fraction| {
                self.remaining_percent
                    .is_some_and(|percent| fraction == percent / 100.0)
            })
            && self.resets_at_ms == window.resets_at_ms
            && self.provider_reset_time.is_none()
            && self.exhausted == window.exhausted
    }
}

/// A more specific provider-defined scope attached to one quota bucket.
/// Current adapters leave this absent unless the provider contract identifies
/// such a scope; this type alone does not authorize model-level scheduling.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum QuotaBucketScope {
    ModelGroup { fingerprint: String },
}

impl QuotaBucketScope {
    fn validate(&self) -> Result<()> {
        match self {
            Self::ModelGroup { fingerprint } => ensure!(
                versioned_sha256(fingerprint, "mg1:"),
                "invalid provider model-group fingerprint"
            ),
        }
        Ok(())
    }
}

/// Explicit provider bucket identity and its sibling windows. Bucket IDs are
/// domain-separated fingerprints; raw provider identifiers are not retained.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaBucket {
    pub provider_bucket_fingerprint: String,
    /// Optional provider-supplied user-facing quota label. Opaque provider IDs
    /// remain fingerprint-only and are never stored here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<QuotaBucketScope>,
    #[serde(default)]
    pub windows: Vec<QuotaBucketWindow>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderScopeEvidenceState {
    Unbound,
    Unconfirmed,
    Confirmed,
    Mismatch,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderIdentityValueComparison {
    #[default]
    NotComparable,
    ExactValueMatch,
    ExactValueMismatch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaEvidencePromotion {
    ObservedUnconfirmed,
    Promoted,
    WithheldScopeMismatch,
    WithheldIdentityMismatch,
    WithheldIdentityUnverified,
    NoUsableQuota,
}

/// Safe, credential-scoped status metadata persisted alongside normalized
/// quota values. This records the trust decision without retaining provider
/// account identifiers or the raw response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderStatusObservation {
    pub scope_state: ProviderScopeEvidenceState,
    pub identity_value_comparison: ProviderIdentityValueComparison,
    pub quota_promotion: QuotaEvidencePromotion,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ordinary_usage_allowed: Option<bool>,
}

/// Basis used to derive an Orbit identity for a provider-defined quota group.
/// Provider identifiers are fingerprinted in memory; display/member metadata
/// is the fallback and does not assert provider-side identity stability.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderQuotaGroupIdentityBasis {
    ProviderIdentifier,
    MemberSet,
    DisplayName,
    /// Orbit-derived fingerprint over the normalized provider display name
    /// and sorted provider member labels; not a provider-issued identifier.
    DisplayNameAndMemberSet,
}

/// A provider-reported model/member label within a shared quota group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderQuotaMember {
    pub provider_label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_key_fingerprint: Option<String>,
}

impl ProviderQuotaMember {
    fn validate(&self) -> Result<()> {
        ensure!(
            safe_provider_label(&self.provider_label, 128),
            "invalid provider quota member label"
        );
        ensure!(
            self.provider_key_fingerprint
                .as_deref()
                .is_none_or(|value| versioned_sha256(value, "qgm1:")),
            "invalid provider quota member fingerprint"
        );
        Ok(())
    }
}

/// Provider-defined group metadata linked to the existing qb1 bucket records.
/// Windows remain nested under those bucket records, so enrichment does not
/// alter or regenerate established quota identities.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderQuotaGroup {
    pub fingerprint: String,
    pub identity_basis: ProviderQuotaGroupIdentityBasis,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_description: Option<String>,
    #[serde(default)]
    pub members: Vec<ProviderQuotaMember>,
    #[serde(default)]
    pub bucket_fingerprints: Vec<String>,
}

impl ProviderQuotaGroup {
    fn validate(&self) -> Result<()> {
        ensure!(
            versioned_sha256(&self.fingerprint, "qg1:"),
            "invalid provider quota group fingerprint"
        );
        ensure!(
            self.provider_display_name
                .as_deref()
                .is_none_or(|value| safe_provider_label(value, 128)),
            "invalid provider quota group display name"
        );
        ensure!(
            self.provider_description
                .as_deref()
                .is_none_or(|value| { safe_provider_label(value, 512) }),
            "invalid provider quota group description"
        );
        ensure!(
            self.provider_display_name.is_some() || !self.members.is_empty(),
            "provider quota group lacks display metadata"
        );
        ensure!(self.members.len() <= 32, "too many provider quota members");
        let mut labels = std::collections::BTreeSet::new();
        let mut keys = std::collections::BTreeSet::new();
        for member in &self.members {
            member.validate()?;
            ensure!(
                labels.insert(&member.provider_label),
                "duplicate provider quota member label"
            );
            if let Some(key) = &member.provider_key_fingerprint {
                ensure!(keys.insert(key), "duplicate provider quota member key");
            }
        }
        ensure!(
            !self.bucket_fingerprints.is_empty() && self.bucket_fingerprints.len() <= 16,
            "provider quota group must reference bounded quota buckets"
        );
        let mut buckets = std::collections::BTreeSet::new();
        for fingerprint in &self.bucket_fingerprints {
            ensure!(
                versioned_sha256(fingerprint, "qb1:"),
                "invalid provider quota group bucket reference"
            );
            ensure!(
                buckets.insert(fingerprint),
                "duplicate quota bucket reference in provider group"
            );
        }
        Ok(())
    }
}

fn safe_provider_label(value: &str, max_bytes: usize) -> bool {
    !value.trim().is_empty()
        && value.len() <= max_bytes
        && !value.chars().any(char::is_control)
        && !value.contains('@')
        && !value.contains("://")
        && value.split_whitespace().all(|word| word.len() <= 40)
        && (value.chars().any(char::is_whitespace) || value.len() <= 40)
}

impl QuotaBucket {
    fn validate(&self) -> Result<()> {
        ensure!(
            versioned_sha256(&self.provider_bucket_fingerprint, "qb1:"),
            "invalid provider quota bucket fingerprint"
        );
        ensure!(
            self.provider_label
                .as_deref()
                .is_none_or(safe_provider_quota_label),
            "invalid provider quota label"
        );
        if let Some(scope) = &self.scope {
            scope.validate()?;
        }
        ensure!(self.windows.len() <= 32, "too many windows in quota bucket");
        let mut identities = std::collections::BTreeSet::new();
        for window in &self.windows {
            window.validate()?;
            ensure!(
                identities.insert(&window.provider_window_id),
                "duplicate provider window identity in quota bucket"
            );
        }
        Ok(())
    }
}

fn safe_provider_quota_label(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= 128
        && value.trim() == value
        && !value.chars().any(char::is_control)
        && !value.contains('@')
        && !value.contains("://")
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AvailabilitySnapshot {
    pub applies_to: AvailabilityScope,
    pub observed_at_ms: i64,
    pub expires_at_ms: i64,
    pub state: AvailabilityState,
    #[serde(default)]
    pub quota_windows: Vec<QuotaWindow>,
    /// Structured bucket hierarchy. Empty/missing means this snapshot uses the
    /// legacy flat window representation and remains backward compatible.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quota_buckets: Vec<QuotaBucket>,
    /// Optional provider group semantics, additive to legacy snapshots and
    /// linked to existing quota bucket fingerprints.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quota_groups: Vec<ProviderQuotaGroup>,
    pub source: EvidenceSource,
    pub confidence: EvidenceConfidence,
    pub source_revision: String,
    pub evidence_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_observed_at_ms: Option<i64>,
    /// Codex-safe status metadata. Old snapshots omit this field and remain
    /// readable; unknown/unconfirmed observations can still retain quota data
    /// while their effective availability remains UNKNOWN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_status_observation: Option<ProviderStatusObservation>,
}

impl AvailabilitySnapshot {
    pub fn validate(&self) -> Result<()> {
        self.applies_to.validate()?;
        ensure!(
            self.observed_at_ms >= 0 && self.expires_at_ms > self.observed_at_ms,
            "invalid availability observation or expiry"
        );
        ensure!(
            self.provider_observed_at_ms.is_none_or(|v| v >= 0),
            "invalid provider observation time"
        );
        ensure!(
            logical_id(&self.source_revision),
            "invalid evidence source revision"
        );
        ensure!(
            sha256_digest(&self.evidence_digest),
            "invalid evidence digest"
        );
        ensure!(self.quota_windows.len() <= 32, "too many quota windows");
        for window in &self.quota_windows {
            window.validate()?;
        }
        ensure!(self.quota_buckets.len() <= 16, "too many quota buckets");
        let mut bucket_fingerprints = std::collections::BTreeSet::new();
        let mut bucket_windows = Vec::new();
        for bucket in &self.quota_buckets {
            bucket.validate()?;
            ensure!(
                bucket_fingerprints.insert(&bucket.provider_bucket_fingerprint),
                "duplicate provider quota bucket fingerprint"
            );
            bucket_windows.extend(bucket.windows.iter());
        }
        ensure!(
            self.quota_groups.len() <= 16,
            "too many provider quota groups"
        );
        let mut group_fingerprints = std::collections::BTreeSet::new();
        let mut referenced_buckets = std::collections::BTreeSet::new();
        for group in &self.quota_groups {
            group.validate()?;
            ensure!(
                group_fingerprints.insert(&group.fingerprint),
                "duplicate provider quota group fingerprint"
            );
            for fingerprint in &group.bucket_fingerprints {
                ensure!(
                    bucket_fingerprints.contains(fingerprint),
                    "provider quota group references an absent bucket"
                );
                ensure!(
                    referenced_buckets.insert(fingerprint),
                    "quota bucket belongs to multiple provider groups"
                );
            }
        }
        ensure!(bucket_windows.len() <= 32, "too many grouped quota windows");
        // New snapshots keep `quota_windows` as a legacy compatibility
        // projection. If both views are present, reject divergent measurements.
        if !self.quota_buckets.is_empty() && !self.quota_windows.is_empty() {
            ensure!(
                bucket_windows.len() == self.quota_windows.len(),
                "grouped and flat quota window counts differ"
            );
            let mut unmatched = self.quota_windows.iter().collect::<Vec<_>>();
            for bucket_window in bucket_windows {
                let Some(index) = unmatched
                    .iter()
                    .position(|window| bucket_window.matches_legacy(window))
                else {
                    anyhow::bail!("grouped and flat quota window values differ");
                };
                unmatched.swap_remove(index);
            }
            ensure!(unmatched.is_empty(), "unmatched legacy quota windows");
        }
        ensure!(
            self.source != EvidenceSource::OperatorOverride
                || self.state != AvailabilityState::Ready,
            "operator override cannot assert provider readiness"
        );
        ensure!(
            match self.source {
                EvidenceSource::ProviderNativeStatus | EvidenceSource::RuntimeNativeStatus => {
                    matches!(
                        self.confidence,
                        EvidenceConfidence::AuthoritativeNative | EvidenceConfidence::Unknown
                    )
                }
                EvidenceSource::ExecutionResult => {
                    self.confidence == EvidenceConfidence::ExecutionObserved
                }
                EvidenceSource::OperatorOverride => {
                    self.confidence == EvidenceConfidence::OperatorAsserted
                }
            },
            "evidence confidence does not match its source"
        );
        Ok(())
    }

    pub fn id(&self) -> Result<String> {
        self.validate()?;
        Ok(format!(
            "av1:{}",
            crate::model::digest(&serde_json::to_vec(&(
                "orbit.availability_snapshot.v1",
                self
            ))?)
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailabilityDecision {
    pub state: AvailabilityState,
    pub evidence_ids: Vec<String>,
    pub stale_ids: Vec<String>,
}

/// Conservative evidence evaluation. A broad positive never proves a specific
/// model ready. Any fresh scoped negative blocks until it expires or a later
/// version replaces that scope's current snapshot.
pub fn effective_at(
    resource: &ExecutionResourceIdentity,
    snapshots: &[AvailabilitySnapshot],
    now_ms: i64,
) -> Result<AvailabilityDecision> {
    resource.validate()?;
    ensure!(now_ms >= 0, "invalid evaluation time");
    let mut current = std::collections::BTreeMap::<String, &AvailabilitySnapshot>::new();
    for snapshot in snapshots {
        snapshot.validate()?;
        if !snapshot.applies_to.matches(resource) {
            continue;
        }
        let key = snapshot.applies_to.key()?;
        let replace = match current.get(&key) {
            Some(old) => {
                (snapshot.observed_at_ms, snapshot.id()?) > (old.observed_at_ms, old.id()?)
            }
            None => true,
        };
        if replace {
            current.insert(key, snapshot);
        }
    }
    let mut evidence_ids = Vec::new();
    let mut stale_ids = Vec::new();
    let mut blocking: Option<(i64, String, AvailabilityState)> = None;
    let mut limited = false;
    let mut exact_ready = false;
    for snapshot in current.values() {
        let id = snapshot.id()?;
        if snapshot.expires_at_ms <= now_ms || snapshot.observed_at_ms > now_ms {
            stale_ids.push(id);
            continue;
        }
        evidence_ids.push(id.clone());
        if snapshot.state.blocking() {
            let candidate = (snapshot.observed_at_ms, id, snapshot.state);
            if blocking.as_ref().is_none_or(|old| {
                candidate.0 > old.0 || (candidate.0 == old.0 && candidate.1 > old.1)
            }) {
                blocking = Some(candidate);
            }
        } else if snapshot.state == AvailabilityState::Limited {
            limited = true;
        } else if snapshot.state == AvailabilityState::Ready
            && matches!(snapshot.applies_to, AvailabilityScope::Exact(_))
        {
            exact_ready = true;
        }
    }
    Ok(AvailabilityDecision {
        state: blocking.map_or_else(
            || {
                if limited {
                    AvailabilityState::Limited
                } else if exact_ready {
                    AvailabilityState::Ready
                } else {
                    AvailabilityState::Unknown
                }
            },
            |(_, _, state)| state,
        ),
        evidence_ids,
        stale_ids,
    })
}

/// Internal persistence seam. No public API or scheduler reads this yet.
/// The immutable history and current pointer are committed together; replaying
/// the same evidence is idempotent and older evidence cannot replace newer.
pub struct AvailabilityStore<'a> {
    pool: &'a PgPool,
}

impl<'a> AvailabilityStore<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    pub async fn record(&self, snapshot: &AvailabilitySnapshot) -> Result<String> {
        let mut tx = self.pool.begin().await?;
        let id = Self::record_in_tx(&mut tx, snapshot).await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Return the current credential-scoped snapshot only. This is an
    /// operator/status read and never falls back to provider- or runtime-wide
    /// evidence belonging to another credential.
    pub async fn current_for_credential(
        &self,
        credential: &CredentialIdentity,
    ) -> Result<Option<AvailabilitySnapshot>> {
        credential.validate()?;
        let expected_scope = AvailabilityScope::Credential(credential.clone());
        let key = expected_scope.key()?;
        let row = sqlx::query("SELECT s.id, c.scope_key, s.evidence FROM orbit_availability_current c JOIN orbit_availability_snapshots s ON s.id=c.snapshot_id WHERE c.scope_key=$1")
            .bind(&key)
            .fetch_optional(self.pool)
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let id: String = row.get("id");
        let stored_scope: String = row.get("scope_key");
        let snapshot: AvailabilitySnapshot = serde_json::from_value(row.get("evidence"))?;
        snapshot.validate()?;
        ensure!(
            stored_scope == key && snapshot.applies_to == expected_scope && snapshot.id()? == id,
            "credential availability identity mismatch"
        );
        Ok(Some(snapshot))
    }

    pub(crate) async fn record_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        snapshot: &AvailabilitySnapshot,
    ) -> Result<String> {
        let id = snapshot.id()?;
        let scope_key = snapshot.applies_to.key()?;
        sqlx::query("INSERT INTO orbit_availability_snapshots (id, scope_key, observed_at_ms, expires_at_ms, evidence) VALUES ($1,$2,$3,$4,$5) ON CONFLICT (id) DO NOTHING")
            .bind(&id)
            .bind(&scope_key)
            .bind(snapshot.observed_at_ms)
            .bind(snapshot.expires_at_ms)
            .bind(serde_json::to_value(snapshot)?)
            .execute(&mut **tx).await?;
        sqlx::query("INSERT INTO orbit_availability_current (scope_key, snapshot_id, observed_at_ms) VALUES ($1,$2,$3) ON CONFLICT (scope_key) DO UPDATE SET snapshot_id=EXCLUDED.snapshot_id, observed_at_ms=EXCLUDED.observed_at_ms WHERE (orbit_availability_current.observed_at_ms, orbit_availability_current.snapshot_id) < (EXCLUDED.observed_at_ms, EXCLUDED.snapshot_id)")
            .bind(&scope_key).bind(&id).bind(snapshot.observed_at_ms)
            .execute(&mut **tx).await?;
        Ok(id)
    }

    pub async fn current_for(
        &self,
        resource: &ExecutionResourceIdentity,
    ) -> Result<Vec<AvailabilitySnapshot>> {
        resource.validate()?;
        let keys = vec![
            AvailabilityScope::Exact(resource.clone()).key()?,
            AvailabilityScope::CredentialModel {
                credential: resource.credential.clone(),
                model: resource.model.clone(),
            }
            .key()?,
            AvailabilityScope::Credential(resource.credential.clone()).key()?,
            AvailabilityScope::Runtime(resource.runtime.clone()).key()?,
        ];
        let rows = if let Some(catalog_id) = &resource.credential.catalog_id {
            sqlx::query("SELECT s.id, s.scope_key, c.scope_key AS current_scope_key, s.evidence FROM orbit_availability_current c JOIN orbit_availability_snapshots s ON s.id=c.snapshot_id WHERE c.scope_key = ANY($1) OR (COALESCE(s.evidence #>> '{applies_to,identity,catalog_id}', s.evidence #>> '{applies_to,identity,credential,catalog_id}')=$2 AND COALESCE(s.evidence #>> '{applies_to,identity,generation}', s.evidence #>> '{applies_to,identity,credential,generation}')=$3 AND COALESCE(s.evidence #>> '{applies_to,identity,provider}', s.evidence #>> '{applies_to,identity,credential,provider}')=$4) ORDER BY c.observed_at_ms DESC, c.snapshot_id DESC LIMIT 65")
                .bind(&keys)
                .bind(catalog_id)
                .bind(&resource.credential.generation)
                .bind(&resource.credential.provider)
                .fetch_all(self.pool)
                .await?
        } else {
            sqlx::query("SELECT s.id, s.scope_key, c.scope_key AS current_scope_key, s.evidence FROM orbit_availability_current c JOIN orbit_availability_snapshots s ON s.id=c.snapshot_id WHERE c.scope_key = ANY($1)")
                .bind(&keys)
                .fetch_all(self.pool)
                .await?
        };
        ensure!(
            rows.len() <= 64,
            "availability identity history exceeds bound"
        );
        let mut current =
            std::collections::BTreeMap::<String, (String, AvailabilitySnapshot)>::new();
        for row in rows {
            let snapshot: AvailabilitySnapshot = serde_json::from_value(row.get("evidence"))?;
            let id: String = row.get("id");
            let scope_key: String = row.get("scope_key");
            let current_scope_key: String = row.get("current_scope_key");
            let calculated_scope_key = snapshot.applies_to.key()?;
            let legacy_catalog_key = snapshot
                .applies_to
                .has_catalog_credential()
                .then(|| legacy_scope_key(&snapshot.applies_to))
                .transpose()?;
            let stored_key_matches = scope_key == calculated_scope_key
                || legacy_catalog_key.as_ref() == Some(&scope_key);
            ensure!(
                snapshot.id()? == id
                    && stored_key_matches
                    && scope_key == current_scope_key
                    && snapshot.applies_to.matches(resource),
                "availability evidence identity mismatch"
            );
            let stable_key = stable_scope_key(&snapshot.applies_to)?;
            let replace = current.get(&stable_key).is_none_or(|(old_id, old)| {
                (snapshot.observed_at_ms, &id) > (old.observed_at_ms, old_id)
            });
            if replace {
                current.insert(stable_key, (id, snapshot));
            }
        }
        Ok(current
            .into_values()
            .map(|(_, snapshot)| snapshot)
            .collect())
    }
}
