//! Durable, operator-confirmed binding of a logical credential generation to
//! an opaque provider-reported account scope. This never authorizes dispatch.
use crate::{
    availability::{
        AvailabilitySnapshot, AvailabilityState, AvailabilityStore, CredentialIdentity,
        EvidenceConfidence,
    },
    model::{digest, id},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Row, Transaction};

pub fn fingerprint(provider: &str, account_id: &str) -> Option<String> {
    if provider.is_empty()
        || provider.len() > 128
        || !provider
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:@+-".contains(&b))
        || account_id.is_empty()
        || account_id.len() > 256
        || account_id.trim() != account_id
        || account_id.chars().any(char::is_control)
    {
        return None;
    }
    let bytes = serde_json::to_vec(&("orbit.provider_scope.v1", provider, account_id)).ok()?;
    Some(format!("ps1:{}", digest(&bytes)))
}

fn valid_fingerprint(value: &str) -> bool {
    value.len() == 68
        && value.starts_with("ps1:")
        && value[4..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn credential_key(credential: &CredentialIdentity) -> Result<String> {
    credential.validate()?;
    Ok(format!(
        "pcb1:{}",
        digest(&serde_json::to_vec(&(
            "orbit.provider_scope_credential.v1",
            credential
        ))?)
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingState {
    Unconfirmed,
    Confirmed,
    Mismatch,
}

impl BindingState {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "unconfirmed" => Ok(Self::Unconfirmed),
            "confirmed" => Ok(Self::Confirmed),
            "mismatch" => Ok(Self::Mismatch),
            _ => anyhow::bail!("invalid persisted provider-scope state"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BindingView {
    pub credential: CredentialIdentity,
    pub fingerprint: String,
    pub state: BindingState,
    pub mismatch_fingerprint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BindingEvent {
    pub id: String,
    pub event_kind: String,
    pub fingerprint: String,
    pub actor: String,
    pub snapshot_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationMode {
    Enrollment,
    Confirmed,
}

pub struct RecordedObservation {
    pub binding: Option<BindingView>,
    pub snapshot: AvailabilitySnapshot,
    pub snapshot_id: String,
}

pub struct BindingStore<'a> {
    pool: &'a PgPool,
}

impl<'a> BindingStore<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    pub async fn inspect(&self, credential: &CredentialIdentity) -> Result<Option<BindingView>> {
        let key = credential_key(credential)?;
        let row = sqlx::query("SELECT credential, fingerprint, state, mismatch_fingerprint FROM orbit_provider_scope_bindings WHERE credential_key=$1")
            .bind(key).fetch_optional(self.pool).await?;
        row.map(|row| decode(row, credential)).transpose()
    }

    pub async fn history(&self, credential: &CredentialIdentity) -> Result<Vec<BindingEvent>> {
        let key = credential_key(credential)?;
        let rows = sqlx::query("SELECT id, event_kind, fingerprint, actor, snapshot_id FROM orbit_provider_scope_binding_events WHERE credential_key=$1 ORDER BY recorded_at DESC, id DESC LIMIT 128")
            .bind(key).fetch_all(self.pool).await?;
        rows.into_iter()
            .map(|row| {
                let fingerprint: String = row.get("fingerprint");
                ensure!(
                    valid_fingerprint(&fingerprint),
                    "invalid persisted fingerprint"
                );
                Ok(BindingEvent {
                    id: row.get("id"),
                    event_kind: row.get("event_kind"),
                    fingerprint,
                    actor: row.get("actor"),
                    snapshot_id: row.get("snapshot_id"),
                })
            })
            .collect()
    }

    /// Called only after provider I/O and confirmed runtime/auth cleanup.
    /// The row lock rechecks binding authority and snapshot publication is in
    /// the same transaction. An enrollment observation is always UNKNOWN.
    pub async fn record_observation(
        &self,
        credential: &CredentialIdentity,
        observed: Option<&str>,
        mode: ObservationMode,
        mut snapshot: AvailabilitySnapshot,
    ) -> Result<RecordedObservation> {
        let key = credential_key(credential)?;
        ensure!(
            matches!(&snapshot.applies_to, crate::availability::AvailabilityScope::Credential(c) if c == credential),
            "status snapshot credential scope mismatch"
        );
        snapshot.validate()?;
        ensure!(
            observed.is_none_or(valid_fingerprint),
            "invalid observed fingerprint"
        );
        let mut tx = self.pool.begin().await?;
        let mut transition: Option<(&str, &str)> = None;
        if mode == ObservationMode::Enrollment
            && let Some(value) = observed
        {
            let created = sqlx::query("INSERT INTO orbit_provider_scope_bindings (credential_key, credential, fingerprint, state) VALUES ($1,$2,$3,'unconfirmed') ON CONFLICT DO NOTHING")
                .bind(&key).bind(serde_json::to_value(credential)?).bind(value)
                .execute(&mut *tx).await?.rows_affected() == 1;
            if created {
                transition = Some(("observed", value));
            }
        }
        let row = sqlx::query("SELECT credential, fingerprint, state, mismatch_fingerprint FROM orbit_provider_scope_bindings WHERE credential_key=$1 FOR UPDATE")
            .bind(&key).fetch_optional(&mut *tx).await?;
        let mut binding = row.map(|row| decode(row, credential)).transpose()?;
        if let (Some(view), Some(value)) = (&mut binding, observed)
            && value != view.fingerprint
            && view.mismatch_fingerprint.as_deref() != Some(value)
        {
            sqlx::query("UPDATE orbit_provider_scope_bindings SET state='mismatch', mismatch_fingerprint=$2, updated_at=clock_timestamp() WHERE credential_key=$1")
                    .bind(&key).bind(value).execute(&mut *tx).await?;
            transition = Some(("mismatch", value));
            view.state = BindingState::Mismatch;
            view.mismatch_fingerprint = Some(value.to_owned());
        }
        let trusted = mode == ObservationMode::Confirmed
            && matches!((&binding, observed), (Some(view), Some(value)) if view.state == BindingState::Confirmed && view.fingerprint == value);
        if !trusted {
            snapshot.state = AvailabilityState::Unknown;
            snapshot.confidence = EvidenceConfidence::Unknown;
            snapshot.quota_windows.clear();
            snapshot.quota_buckets.clear();
        }
        // The availability pointer orders by observed_at and ID. Two status
        // reads can share one clock millisecond; a later UNKNOWN must not lose
        // a tie to an earlier READY. This is local logical-time ordering only,
        // never a fabricated provider timestamp or reset.
        let scope_key = snapshot.applies_to.key()?;
        let previous: Option<i64> = sqlx::query_scalar(
            "SELECT observed_at_ms FROM orbit_availability_current WHERE scope_key=$1 FOR UPDATE",
        )
        .bind(&scope_key)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(previous) = previous
            && previous >= snapshot.observed_at_ms
        {
            let next = previous
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("status observation time exhausted"))?;
            let shift = next - snapshot.observed_at_ms;
            snapshot.observed_at_ms = next;
            snapshot.expires_at_ms = snapshot
                .expires_at_ms
                .checked_add(shift)
                .ok_or_else(|| anyhow::anyhow!("status expiry time exhausted"))?;
        }
        let snapshot_id = AvailabilityStore::record_in_tx(&mut tx, &snapshot).await?;
        if let Some((kind, value)) = transition {
            event(
                &mut tx,
                &key,
                kind,
                value,
                "status_probe",
                Some(&snapshot_id),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(RecordedObservation {
            binding,
            snapshot,
            snapshot_id,
        })
    }

    /// Explicit operator action. A second identical confirmation is a no-op;
    /// a different fingerprint or changed state cannot be confirmed by replay.
    pub async fn confirm(
        &self,
        credential: &CredentialIdentity,
        fingerprint: &str,
        actor: &str,
    ) -> Result<BindingView> {
        ensure!(valid_fingerprint(fingerprint), "invalid fingerprint");
        validate_actor(actor)?;
        let key = credential_key(credential)?;
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT credential, fingerprint, state, mismatch_fingerprint FROM orbit_provider_scope_bindings WHERE credential_key=$1 FOR UPDATE")
            .bind(&key).fetch_optional(&mut *tx).await?;
        let mut view = decode(
            row.ok_or_else(|| anyhow::anyhow!("no provider scope to confirm"))?,
            credential,
        )?;
        ensure!(
            view.fingerprint == fingerprint,
            "provider scope fingerprint mismatch"
        );
        ensure!(
            view.state != BindingState::Mismatch,
            "re-enrollment required after mismatch"
        );
        if view.state == BindingState::Unconfirmed {
            sqlx::query("UPDATE orbit_provider_scope_bindings SET state='confirmed', updated_at=clock_timestamp() WHERE credential_key=$1")
                .bind(&key).execute(&mut *tx).await?;
            event(&mut tx, &key, "confirmed", fingerprint, actor, None).await?;
            view.state = BindingState::Confirmed;
        }
        tx.commit().await?;
        Ok(view)
    }

    /// Explicitly discard a mismatched current binding in favor of the last
    /// observed fingerprint. Confirmation remains a separate operator action.
    pub async fn re_enroll(
        &self,
        credential: &CredentialIdentity,
        fingerprint: &str,
        actor: &str,
    ) -> Result<BindingView> {
        ensure!(valid_fingerprint(fingerprint), "invalid fingerprint");
        validate_actor(actor)?;
        let key = credential_key(credential)?;
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT credential, fingerprint, state, mismatch_fingerprint FROM orbit_provider_scope_bindings WHERE credential_key=$1 FOR UPDATE")
            .bind(&key).fetch_optional(&mut *tx).await?;
        let mut view = decode(
            row.ok_or_else(|| anyhow::anyhow!("no provider scope to re-enroll"))?,
            credential,
        )?;
        ensure!(
            view.state == BindingState::Mismatch
                && view.mismatch_fingerprint.as_deref() == Some(fingerprint),
            "mismatched fingerprint is not the observed candidate"
        );
        sqlx::query("UPDATE orbit_provider_scope_bindings SET state='unconfirmed', fingerprint=$2, mismatch_fingerprint=NULL, updated_at=clock_timestamp() WHERE credential_key=$1")
            .bind(&key).bind(fingerprint).execute(&mut *tx).await?;
        event(&mut tx, &key, "reenrolled", fingerprint, actor, None).await?;
        view.fingerprint = fingerprint.to_owned();
        view.mismatch_fingerprint = None;
        view.state = BindingState::Unconfirmed;
        tx.commit().await?;
        Ok(view)
    }
}

fn validate_actor(actor: &str) -> Result<()> {
    ensure!(
        !actor.is_empty()
            && actor.len() <= 128
            && actor
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._:@+-".contains(&b)),
        "invalid operator identity"
    );
    Ok(())
}

fn decode(row: sqlx::postgres::PgRow, expected: &CredentialIdentity) -> Result<BindingView> {
    let credential: CredentialIdentity = serde_json::from_value(row.get("credential"))?;
    ensure!(
        &credential == expected,
        "persisted credential identity mismatch"
    );
    let fingerprint: String = row.get("fingerprint");
    let mismatch_fingerprint: Option<String> = row.get("mismatch_fingerprint");
    let state = BindingState::parse(row.get("state"))?;
    ensure!(
        valid_fingerprint(&fingerprint)
            && mismatch_fingerprint
                .as_deref()
                .is_none_or(valid_fingerprint),
        "invalid persisted fingerprint"
    );
    ensure!(
        (state == BindingState::Mismatch) == mismatch_fingerprint.is_some(),
        "invalid persisted binding state"
    );
    Ok(BindingView {
        credential,
        fingerprint,
        state,
        mismatch_fingerprint,
    })
}

async fn event(
    tx: &mut Transaction<'_, Postgres>,
    key: &str,
    kind: &str,
    fingerprint: &str,
    actor: &str,
    snapshot_id: Option<&str>,
) -> Result<()> {
    sqlx::query("INSERT INTO orbit_provider_scope_binding_events (id, credential_key, event_kind, fingerprint, actor, snapshot_id) VALUES ($1,$2,$3,$4,$5,$6)")
        .bind(id()).bind(key).bind(kind).bind(fingerprint).bind(actor).bind(snapshot_id)
        .execute(&mut **tx).await?;
    Ok(())
}
