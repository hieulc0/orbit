//! Operator-scoped credential catalog. PostgreSQL owns metadata and lifecycle;
//! [`SecretBackend`] owns bytes. No workflow or worker route mutates this store.
use crate::{
    availability::CredentialIdentity,
    model::id,
    secret_backend::{SecretBackend, SecretBytes, SecretLocator},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row, postgres::PgRow};

pub const CONTROL_SCOPE: &str = "operator";

pub fn valid_reference(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:@+-".contains(&b))
}

fn logical(value: &str) -> bool {
    valid_reference(value)
}

fn uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|parsed| parsed.to_string() == value)
}

fn validate_endpoint(value: Option<&str>) -> Result<()> {
    if let Some(value) = value {
        ensure!(
            value.len() <= 2048 && !value.chars().any(char::is_control),
            "invalid credential endpoint"
        );
        let parsed = reqwest::Url::parse(value)
            .map_err(|_| anyhow::anyhow!("invalid credential endpoint"))?;
        ensure!(
            parsed.host_str().is_some()
                && parsed.username().is_empty()
                && parsed.password().is_none()
                && parsed.query().is_none()
                && parsed.fragment().is_none(),
            "invalid credential endpoint"
        );
    }
    Ok(())
}

fn validate_capabilities(capabilities: &[String]) -> Result<()> {
    ensure!(
        capabilities.len() <= 64 && capabilities.iter().all(|v| logical(v)),
        "invalid credential representation capabilities"
    );
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeProvenance {
    /// Logical artifact label; physical executable paths are never catalog data.
    pub artifact: String,
    pub version: String,
    pub sha256: String,
    pub provenance: String,
}

impl RuntimeProvenance {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            logical(&self.artifact)
                && logical(&self.version)
                && self.sha256.len() == 64
                && self
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                && matches!(
                    self.provenance.as_str(),
                    "operator-supplied" | "officially-verified" | "pinned-build"
                ),
            "invalid runtime provenance"
        );
        Ok(())
    }

    fn public(&self) -> RuntimeProvenanceView {
        RuntimeProvenanceView {
            artifact: self.artifact.clone(),
            version: self.version.clone(),
            sha256: self.sha256.clone(),
            provenance: self.provenance.clone(),
        }
    }
}

/// Safe runtime identity for operator-facing inspection; physical executable
/// paths are neither stored in PostgreSQL nor exposed here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RuntimeProvenanceView {
    pub artifact: String,
    pub version: String,
    pub sha256: String,
    pub provenance: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialStatus {
    Pending,
    Enrolled,
    Invalid,
    Disabled,
    Revoked,
}

impl CredentialStatus {
    fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "pending" => Self::Pending,
            "enrolled" => Self::Enrolled,
            "invalid" => Self::Invalid,
            "disabled" => Self::Disabled,
            "revoked" => Self::Revoked,
            _ => anyhow::bail!("invalid persisted credential status"),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepresentationState {
    Pending,
    Stored,
    Invalid,
    Disabled,
    Revoked,
}

impl RepresentationState {
    fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "pending" => Self::Pending,
            "stored" => Self::Stored,
            "invalid" => Self::Invalid,
            "disabled" => Self::Disabled,
            "revoked" => Self::Revoked,
            _ => anyhow::bail!("invalid persisted representation state"),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credential {
    pub id: String,
    pub provider: String,
    pub reference: String,
    pub generation: u64,
    pub endpoint: Option<String>,
    pub auth_type: String,
    pub secret_backend: String,
    pub secret_locator: Option<SecretLocator>,
    pub status: CredentialStatus,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl Credential {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            uuid(&self.id) && logical(&self.secret_backend),
            "invalid credential identity"
        );
        self.identity().validate()?;
        ensure!(
            self.generation > 0 && self.generation <= i64::MAX as u64 && logical(&self.auth_type),
            "invalid credential generation or authentication type"
        );
        validate_endpoint(self.endpoint.as_deref())?;
        ensure!(
            self.secret_locator
                .is_none_or(|locator| locator.belongs_to(&self.id, self.generation))
                && (self.status != CredentialStatus::Enrolled || self.secret_locator.is_some())
                && self.created_at_ms >= 0
                && self.updated_at_ms >= self.created_at_ms,
            "invalid credential catalog state"
        );
        Ok(())
    }

    pub fn identity(&self) -> CredentialIdentity {
        CredentialIdentity {
            provider: self.provider.clone(),
            reference: self.reference.clone(),
            generation: self.generation.to_string(),
            catalog_id: Some(self.id.clone()),
        }
    }

    pub fn public(&self) -> CredentialView {
        CredentialView {
            id: self.id.clone(),
            provider: self.provider.clone(),
            reference: self.reference.clone(),
            generation: self.generation,
            endpoint: self.endpoint.clone(),
            auth_type: self.auth_type.clone(),
            secret_backend: self.secret_backend.clone(),
            status: self.status,
            has_secret: self.secret_locator.is_some(),
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialRepresentation {
    pub id: String,
    pub credential_id: String,
    pub generation: u64,
    pub interface: String,
    pub auth_type: String,
    pub state: RepresentationState,
    pub secret_locator: Option<SecretLocator>,
    pub capabilities: Vec<String>,
    pub runtime_provenance: Option<RuntimeProvenance>,
    pub last_validated_at_ms: Option<i64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl CredentialRepresentation {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            uuid(&self.id)
                && uuid(&self.credential_id)
                && self.generation > 0
                && logical(&self.interface)
                && logical(&self.auth_type),
            "invalid credential representation identity"
        );
        validate_capabilities(&self.capabilities)?;
        if let Some(provenance) = &self.runtime_provenance {
            provenance.validate()?;
        }
        ensure!(
            self.secret_locator
                .is_none_or(|locator| locator.belongs_to(&self.credential_id, self.generation))
                && (self.state != RepresentationState::Stored || self.secret_locator.is_some())
                && self.created_at_ms >= 0
                && self.updated_at_ms >= self.created_at_ms,
            "invalid credential representation state"
        );
        Ok(())
    }

    pub fn public(&self, current_generation: u64) -> RepresentationView {
        RepresentationView {
            id: self.id.clone(),
            generation: self.generation,
            current_generation: self.generation == current_generation,
            interface: self.interface.clone(),
            auth_type: self.auth_type.clone(),
            state: self.state,
            validation: match (self.state, self.last_validated_at_ms.is_some()) {
                (RepresentationState::Stored, true) => "valid",
                (RepresentationState::Invalid, _) => "invalid",
                _ => "unvalidated",
            }
            .to_owned(),
            capabilities: self.capabilities.clone(),
            runtime_provenance: self
                .runtime_provenance
                .as_ref()
                .map(RuntimeProvenance::public),
            has_secret: self.secret_locator.is_some(),
            last_validated_at_ms: self.last_validated_at_ms,
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
        }
    }
}

/// Operator-facing JSON deliberately excludes every locator and all bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CredentialView {
    pub id: String,
    pub provider: String,
    pub reference: String,
    pub generation: u64,
    pub endpoint: Option<String>,
    pub auth_type: String,
    pub secret_backend: String,
    pub status: CredentialStatus,
    pub has_secret: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RepresentationView {
    pub id: String,
    pub generation: u64,
    pub current_generation: bool,
    pub interface: String,
    pub auth_type: String,
    pub state: RepresentationState,
    /// Validation is separate from durable publication state.
    pub validation: String,
    pub capabilities: Vec<String>,
    pub runtime_provenance: Option<RuntimeProvenanceView>,
    pub has_secret: bool,
    pub last_validated_at_ms: Option<i64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GenerationView {
    pub generation: u64,
    pub secret_backend: String,
    pub state: String,
    pub has_secret: bool,
    pub created_at_ms: i64,
    pub retired_at_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CredentialInspection {
    pub credential: CredentialView,
    pub generations: Vec<GenerationView>,
    pub representations: Vec<RepresentationView>,
    pub identity_bindings: Vec<CredentialIdentityBindingView>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CredentialIdentityBindingView {
    pub generation: u64,
    pub current_generation: bool,
    pub interface_a: String,
    pub interface_b: String,
    pub state: String,
    pub basis: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

const CREDENTIAL_COLUMNS: &str = "c.id, c.provider, c.reference, c.current_generation AS generation, c.endpoint, c.auth_type, c.status, g.backend, g.secret_locator, floor(extract(epoch FROM c.created_at)*1000)::bigint AS created_at_ms, floor(extract(epoch FROM c.updated_at)*1000)::bigint AS updated_at_ms";
const REPRESENTATION_COLUMNS: &str = "id, credential_id, generation, interface, auth_type, state, secret_locator, capabilities, runtime_provenance, floor(extract(epoch FROM last_validated_at)*1000)::bigint AS last_validated_at_ms, floor(extract(epoch FROM created_at)*1000)::bigint AS created_at_ms, floor(extract(epoch FROM updated_at)*1000)::bigint AS updated_at_ms";

fn decode_credential(row: PgRow) -> Result<Credential> {
    let locator: Option<String> = row.get("secret_locator");
    let credential = Credential {
        id: row.get("id"),
        provider: row.get("provider"),
        reference: row.get("reference"),
        generation: row.get::<i64, _>("generation") as u64,
        endpoint: row.get("endpoint"),
        auth_type: row.get("auth_type"),
        secret_backend: row.get("backend"),
        secret_locator: locator.as_deref().map(SecretLocator::parse).transpose()?,
        status: CredentialStatus::parse(row.get("status"))?,
        created_at_ms: row.get("created_at_ms"),
        updated_at_ms: row.get("updated_at_ms"),
    };
    credential.validate()?;
    Ok(credential)
}

fn decode_representation(row: PgRow) -> Result<CredentialRepresentation> {
    let locator: Option<String> = row.get("secret_locator");
    let representation = CredentialRepresentation {
        id: row.get("id"),
        credential_id: row.get("credential_id"),
        generation: row.get::<i64, _>("generation") as u64,
        interface: row.get("interface"),
        auth_type: row.get("auth_type"),
        state: RepresentationState::parse(row.get("state"))?,
        secret_locator: locator.as_deref().map(SecretLocator::parse).transpose()?,
        capabilities: row.get("capabilities"),
        runtime_provenance: row
            .get::<Option<serde_json::Value>, _>("runtime_provenance")
            .map(serde_json::from_value)
            .transpose()?,
        last_validated_at_ms: row.get("last_validated_at_ms"),
        created_at_ms: row.get("created_at_ms"),
        updated_at_ms: row.get("updated_at_ms"),
    };
    representation.validate()?;
    Ok(representation)
}

pub struct CredentialStore<'a> {
    pool: &'a PgPool,
}

impl<'a> CredentialStore<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    pub async fn create(
        &self,
        provider: &str,
        reference: &str,
        endpoint: Option<&str>,
        auth_type: &str,
        backend: &str,
    ) -> Result<Credential> {
        ensure!(
            logical(provider) && logical(reference) && logical(auth_type) && logical(backend),
            "invalid credential metadata"
        );
        validate_endpoint(endpoint)?;
        let credential_id = id();
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO orbit_credentials(id, scope_key, provider, reference, current_generation, endpoint, auth_type, status) VALUES($1,$2,$3,$4,1,$5,$6,'pending')")
            .bind(&credential_id).bind(CONTROL_SCOPE).bind(provider).bind(reference).bind(endpoint).bind(auth_type)
            .execute(&mut *tx).await?;
        sqlx::query("INSERT INTO orbit_credential_generations(credential_id, generation, backend, state) VALUES($1,1,$2,'pending')")
            .bind(&credential_id).bind(backend).execute(&mut *tx).await?;
        tx.commit().await?;
        self.get(reference)
            .await?
            .context("created credential missing")
    }

    pub async fn get(&self, reference: &str) -> Result<Option<Credential>> {
        ensure!(logical(reference), "invalid credential reference");
        let query = format!(
            "SELECT {CREDENTIAL_COLUMNS} FROM orbit_credentials c JOIN orbit_credential_generations g ON g.credential_id=c.id AND g.generation=c.current_generation WHERE c.scope_key=$1 AND c.reference=$2"
        );
        sqlx::query(&query)
            .bind(CONTROL_SCOPE)
            .bind(reference)
            .fetch_optional(self.pool)
            .await?
            .map(decode_credential)
            .transpose()
    }

    pub async fn list(&self) -> Result<Vec<CredentialView>> {
        let query = format!(
            "SELECT {CREDENTIAL_COLUMNS} FROM orbit_credentials c JOIN orbit_credential_generations g ON g.credential_id=c.id AND g.generation=c.current_generation WHERE c.scope_key=$1 ORDER BY c.reference, c.id LIMIT 1025"
        );
        let rows = sqlx::query(&query)
            .bind(CONTROL_SCOPE)
            .fetch_all(self.pool)
            .await?;
        ensure!(rows.len() <= 1024, "credential list exceeds bound");
        rows.into_iter()
            .map(decode_credential)
            .map(|row| row.map(|c| c.public()))
            .collect()
    }

    pub async fn inspect(&self, reference: &str) -> Result<Option<CredentialInspection>> {
        let Some(credential) = self.get(reference).await? else {
            return Ok(None);
        };
        let generation_rows = sqlx::query("SELECT generation, backend, state, secret_locator, floor(extract(epoch FROM created_at)*1000)::bigint AS created_at_ms, floor(extract(epoch FROM retired_at)*1000)::bigint AS retired_at_ms FROM orbit_credential_generations WHERE credential_id=$1 ORDER BY generation LIMIT 1025")
            .bind(&credential.id).fetch_all(self.pool).await?;
        ensure!(
            generation_rows.len() <= 1024,
            "credential generation list exceeds bound"
        );
        let generations = generation_rows
            .into_iter()
            .map(|row| GenerationView {
                generation: row.get::<i64, _>("generation") as u64,
                secret_backend: row.get("backend"),
                state: row.get("state"),
                has_secret: row.get::<Option<String>, _>("secret_locator").is_some(),
                created_at_ms: row.get("created_at_ms"),
                retired_at_ms: row.get("retired_at_ms"),
            })
            .collect();
        let query = format!(
            "SELECT {REPRESENTATION_COLUMNS} FROM orbit_credential_representations WHERE credential_id=$1 ORDER BY generation, interface, id LIMIT 1025"
        );
        let rows = sqlx::query(&query)
            .bind(&credential.id)
            .fetch_all(self.pool)
            .await?;
        ensure!(
            rows.len() <= 1024,
            "credential representation list exceeds bound"
        );
        let representations = rows
            .into_iter()
            .map(decode_representation)
            .map(|row| row.map(|r| r.public(credential.generation)))
            .collect::<Result<Vec<_>>>()?;
        let binding_rows = sqlx::query("SELECT generation, interface_a, interface_b, state, basis, floor(extract(epoch FROM created_at)*1000)::bigint AS created_at_ms, floor(extract(epoch FROM updated_at)*1000)::bigint AS updated_at_ms FROM orbit_credential_identity_bindings WHERE credential_id=$1 ORDER BY generation, interface_a, interface_b LIMIT 1025")
            .bind(&credential.id).fetch_all(self.pool).await?;
        ensure!(
            binding_rows.len() <= 1024,
            "credential identity binding list exceeds bound"
        );
        let identity_bindings = binding_rows
            .into_iter()
            .map(|row| {
                let generation = row.get::<i64, _>("generation") as u64;
                CredentialIdentityBindingView {
                    generation,
                    current_generation: generation == credential.generation,
                    interface_a: row.get("interface_a"),
                    interface_b: row.get("interface_b"),
                    state: row.get("state"),
                    basis: row.get("basis"),
                    created_at_ms: row.get("created_at_ms"),
                    updated_at_ms: row.get("updated_at_ms"),
                }
            })
            .collect();
        Ok(Some(CredentialInspection {
            credential: credential.public(),
            generations,
            representations,
            identity_bindings,
        }))
    }

    /// Record that the operator intended two interfaces to use one provider
    /// account. This deliberately cannot verify provider identity.
    pub async fn record_operator_intended_identity_binding(
        &self,
        reference: &str,
        interface_one: &str,
        interface_two: &str,
    ) -> Result<CredentialIdentityBindingView> {
        ensure!(
            logical(reference)
                && logical(interface_one)
                && logical(interface_two)
                && interface_one != interface_two,
            "invalid credential identity binding"
        );
        let (interface_a, interface_b) = if interface_one < interface_two {
            (interface_one, interface_two)
        } else {
            (interface_two, interface_one)
        };
        let mut tx = self.pool.begin().await?;
        let credential = sqlx::query("SELECT id, current_generation, status FROM orbit_credentials WHERE scope_key='operator' AND reference=$1 FOR UPDATE")
            .bind(reference).fetch_optional(&mut *tx).await?.context("credential not found")?;
        ensure!(
            credential.get::<&str, _>("status") == "enrolled",
            "identity binding requires an enrolled credential"
        );
        let credential_id: String = credential.get("id");
        let generation: i64 = credential.get("current_generation");
        let validated_count: i64 = sqlx::query_scalar("SELECT count(*) FROM orbit_credential_representations WHERE credential_id=$1 AND generation=$2 AND interface IN ($3,$4) AND state='stored' AND last_validated_at IS NOT NULL")
            .bind(&credential_id).bind(generation).bind(interface_a).bind(interface_b).fetch_one(&mut *tx).await?;
        ensure!(
            validated_count == 2,
            "identity binding requires two validated representations"
        );
        sqlx::query("INSERT INTO orbit_credential_identity_bindings(credential_id,generation,interface_a,interface_b,state,basis) VALUES($1,$2,$3,$4,'unverified','operator-intent') ON CONFLICT (credential_id,generation,interface_a,interface_b) DO NOTHING")
            .bind(&credential_id).bind(generation).bind(interface_a).bind(interface_b).execute(&mut *tx).await?;
        let row = sqlx::query("SELECT generation, interface_a, interface_b, state, basis, floor(extract(epoch FROM created_at)*1000)::bigint AS created_at_ms, floor(extract(epoch FROM updated_at)*1000)::bigint AS updated_at_ms FROM orbit_credential_identity_bindings WHERE credential_id=$1 AND generation=$2 AND interface_a=$3 AND interface_b=$4")
            .bind(&credential_id).bind(generation).bind(interface_a).bind(interface_b).fetch_one(&mut *tx).await?;
        let binding = CredentialIdentityBindingView {
            generation: row.get::<i64, _>("generation") as u64,
            current_generation: true,
            interface_a: row.get("interface_a"),
            interface_b: row.get("interface_b"),
            state: row.get("state"),
            basis: row.get("basis"),
            created_at_ms: row.get("created_at_ms"),
            updated_at_ms: row.get("updated_at_ms"),
        };
        tx.commit().await?;
        Ok(binding)
    }

    pub async fn update_pending_metadata(
        &self,
        reference: &str,
        endpoint: Option<&str>,
        auth_type: &str,
    ) -> Result<Credential> {
        ensure!(
            logical(reference) && logical(auth_type),
            "invalid credential metadata"
        );
        validate_endpoint(endpoint)?;
        let updated = sqlx::query("UPDATE orbit_credentials SET endpoint=$2, auth_type=$3, updated_at=clock_timestamp() WHERE scope_key='operator' AND reference=$1 AND status='pending' AND NOT EXISTS (SELECT 1 FROM orbit_credential_representations r WHERE r.credential_id=orbit_credentials.id AND r.generation=orbit_credentials.current_generation) RETURNING id")
            .bind(reference).bind(endpoint).bind(auth_type).fetch_optional(self.pool).await?;
        ensure!(updated.is_some(), "credential is not pending");
        self.get(reference)
            .await?
            .context("updated credential missing")
    }

    /// Persist a pending locator before any filesystem write. A crash or
    /// backend error leaves metadata inert and gives recovery/GC a locator.
    pub async fn prepare_representation(
        &self,
        reference: &str,
        interface: &str,
        capabilities: &[String],
        backend: &str,
    ) -> Result<CredentialRepresentation> {
        let auth_type: String = sqlx::query_scalar(
            "SELECT auth_type FROM orbit_credentials WHERE scope_key='operator' AND reference=$1",
        )
        .bind(reference)
        .fetch_optional(self.pool)
        .await?
        .context("credential not found")?;
        self.prepare_representation_with_metadata(
            reference,
            interface,
            &auth_type,
            capabilities,
            backend,
            None,
        )
        .await
    }

    pub async fn prepare_representation_with_metadata(
        &self,
        reference: &str,
        interface: &str,
        auth_type: &str,
        capabilities: &[String],
        backend: &str,
        runtime_provenance: Option<&RuntimeProvenance>,
    ) -> Result<CredentialRepresentation> {
        ensure!(
            logical(reference) && logical(interface) && logical(auth_type) && logical(backend),
            "invalid representation metadata"
        );
        validate_capabilities(capabilities)?;
        if let Some(provenance) = runtime_provenance {
            provenance.validate()?;
        }
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT c.id, c.current_generation, c.status, g.backend FROM orbit_credentials c JOIN orbit_credential_generations g ON g.credential_id=c.id AND g.generation=c.current_generation WHERE c.scope_key='operator' AND c.reference=$1 FOR UPDATE OF c")
            .bind(reference).fetch_optional(&mut *tx).await?.context("credential not found")?;
        let state: &str = row.get("status");
        ensure!(
            matches!(state, "pending" | "enrolled"),
            "credential cannot accept representations"
        );
        ensure!(
            row.get::<&str, _>("backend") == backend,
            "secret backend mismatch"
        );
        let credential_id: String = row.get("id");
        let generation: i64 = row.get("current_generation");
        let representation_id = id();
        let locator = SecretLocator::new(&credential_id, generation as u64, &representation_id)?;
        let provenance = runtime_provenance.map(serde_json::to_value).transpose()?;
        sqlx::query("INSERT INTO orbit_credential_representations(id, credential_id, generation, interface, auth_type, state, secret_locator, capabilities, runtime_provenance) VALUES($1,$2,$3,$4,$5,'pending',$6,$7,$8)")
            .bind(&representation_id).bind(&credential_id).bind(generation).bind(interface).bind(auth_type).bind(locator.encode()).bind(capabilities).bind(provenance)
            .execute(&mut *tx).await?;
        tx.commit().await?;
        self.representation(&representation_id)
            .await?
            .context("pending representation missing")
    }

    pub async fn representation(&self, id: &str) -> Result<Option<CredentialRepresentation>> {
        ensure!(uuid(id), "invalid representation ID");
        let query = format!(
            "SELECT {REPRESENTATION_COLUMNS} FROM orbit_credential_representations WHERE id=$1"
        );
        sqlx::query(&query)
            .bind(id)
            .fetch_optional(self.pool)
            .await?
            .map(decode_representation)
            .transpose()
    }

    /// Backend I/O completes before the database row lock is taken. Retrying
    /// this operation is safe after a DB failure if the pending file exists.
    pub async fn finalize_representation(
        &self,
        backend: &dyn SecretBackend,
        representation_id: &str,
    ) -> Result<CredentialRepresentation> {
        self.finalize_representation_inner(backend, representation_id, false)
            .await
    }

    /// Used only after a provider adapter has validated the stored secret in
    /// an independent fresh runtime. The validation timestamp and activation
    /// commit together; generic byte-only provisioning remains unchanged.
    pub async fn finalize_validated_representation(
        &self,
        backend: &dyn SecretBackend,
        representation_id: &str,
    ) -> Result<CredentialRepresentation> {
        self.finalize_representation_inner(backend, representation_id, true)
            .await
    }

    async fn finalize_representation_inner(
        &self,
        backend: &dyn SecretBackend,
        representation_id: &str,
        validated: bool,
    ) -> Result<CredentialRepresentation> {
        let prepared = self
            .representation(representation_id)
            .await?
            .context("representation not found")?;
        let locator = prepared
            .secret_locator
            .context("pending representation has no locator")?;
        ensure!(
            backend.exists(locator).await?,
            "private secret is not durable"
        );
        let mut tx = self.pool.begin().await?;
        let credential = sqlx::query("SELECT c.current_generation, c.status, g.backend FROM orbit_credentials c JOIN orbit_credential_generations g ON g.credential_id=c.id AND g.generation=c.current_generation WHERE c.id=$1 FOR UPDATE OF c")
            .bind(&prepared.credential_id).fetch_one(&mut *tx).await?;
        let generation: i64 = credential.get("current_generation");
        ensure!(
            generation as u64 == prepared.generation,
            "credential generation changed"
        );
        ensure!(
            credential.get::<&str, _>("backend") == backend.backend_id(),
            "secret backend mismatch"
        );
        ensure!(
            matches!(credential.get::<&str, _>("status"), "pending" | "enrolled"),
            "credential is not active for enrollment"
        );
        let row = sqlx::query("SELECT state, secret_locator, last_validated_at IS NOT NULL AS was_validated FROM orbit_credential_representations WHERE id=$1 FOR UPDATE")
            .bind(representation_id).fetch_one(&mut *tx).await?;
        let state: &str = row.get("state");
        let current_locator: Option<String> = row.get("secret_locator");
        ensure!(
            matches!(state, "pending" | "stored")
                && current_locator.as_deref() == Some(locator.encode().as_str()),
            "representation changed"
        );
        let was_validated: bool = row.get("was_validated");
        if state == "pending" {
            sqlx::query("UPDATE orbit_credential_representations SET state='stored', last_validated_at=CASE WHEN $2 THEN clock_timestamp() ELSE NULL END, updated_at=clock_timestamp() WHERE id=$1")
                .bind(representation_id).bind(validated).execute(&mut *tx).await?;
            sqlx::query("UPDATE orbit_credential_generations SET state='enrolled', secret_locator=COALESCE(secret_locator,$3) WHERE credential_id=$1 AND generation=$2")
                .bind(&prepared.credential_id).bind(generation).bind(locator.encode()).execute(&mut *tx).await?;
            sqlx::query("UPDATE orbit_credentials SET status='enrolled', updated_at=clock_timestamp() WHERE id=$1")
                .bind(&prepared.credential_id).execute(&mut *tx).await?;
        } else if validated && !was_validated {
            sqlx::query("UPDATE orbit_credential_representations SET last_validated_at=clock_timestamp(), updated_at=clock_timestamp() WHERE id=$1")
                .bind(representation_id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        self.representation(representation_id)
            .await?
            .context("finalized representation missing")
    }

    pub async fn provision_representation(
        &self,
        backend: &dyn SecretBackend,
        reference: &str,
        interface: &str,
        capabilities: &[String],
        secret: SecretBytes,
    ) -> Result<CredentialRepresentation> {
        let prepared = self
            .prepare_representation(reference, interface, capabilities, backend.backend_id())
            .await?;
        let locator = prepared
            .secret_locator
            .context("pending representation has no locator")?;
        backend.create(locator, secret).await?;
        self.finalize_representation(backend, &prepared.id).await
    }

    /// A second interface may refer to the same stored secret in the *current*
    /// generation. No bytes or provider identity are copied or inferred.
    pub async fn link_representation(
        &self,
        backend: &dyn SecretBackend,
        reference: &str,
        source_id: &str,
        interface: &str,
        capabilities: &[String],
    ) -> Result<CredentialRepresentation> {
        ensure!(
            logical(reference) && logical(interface) && uuid(source_id),
            "invalid representation metadata"
        );
        validate_capabilities(capabilities)?;
        let source = self
            .representation(source_id)
            .await?
            .context("source representation missing")?;
        let locator = source
            .secret_locator
            .context("source representation has no secret")?;
        ensure!(
            source.state == RepresentationState::Stored && backend.exists(locator).await?,
            "source representation is not stored"
        );
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT c.id, c.current_generation, c.status, g.backend FROM orbit_credentials c JOIN orbit_credential_generations g ON g.credential_id=c.id AND g.generation=c.current_generation WHERE c.scope_key='operator' AND c.reference=$1 FOR UPDATE OF c")
            .bind(reference).fetch_one(&mut *tx).await?;
        let credential_id: String = row.get("id");
        let generation: i64 = row.get("current_generation");
        ensure!(
            credential_id == source.credential_id
                && generation as u64 == source.generation
                && row.get::<&str, _>("backend") == backend.backend_id()
                && row.get::<&str, _>("status") == "enrolled",
            "source credential generation changed"
        );
        let source_state: String = sqlx::query_scalar(
            "SELECT state FROM orbit_credential_representations WHERE id=$1 FOR UPDATE",
        )
        .bind(source_id)
        .fetch_one(&mut *tx)
        .await?;
        ensure!(source_state == "stored", "source representation changed");
        let linked_id = id();
        let provenance = source
            .runtime_provenance
            .as_ref()
            .map(serde_json::to_value)
            .transpose()?;
        sqlx::query("INSERT INTO orbit_credential_representations(id, credential_id, generation, interface, auth_type, state, secret_locator, capabilities, runtime_provenance) VALUES($1,$2,$3,$4,$5,'stored',$6,$7,$8)")
            .bind(&linked_id).bind(&credential_id).bind(generation).bind(interface).bind(&source.auth_type).bind(locator.encode()).bind(capabilities).bind(provenance)
            .execute(&mut *tx).await?;
        tx.commit().await?;
        self.representation(&linked_id)
            .await?
            .context("linked representation missing")
    }

    /// The old generation remains recorded and its secret stays in the backend,
    /// but it no longer supplies the current identity or validation state.
    pub async fn rotate(&self, reference: &str) -> Result<Credential> {
        ensure!(logical(reference), "invalid credential reference");
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT id, current_generation, status FROM orbit_credentials WHERE scope_key='operator' AND reference=$1 FOR UPDATE")
            .bind(reference).fetch_one(&mut *tx).await?;
        ensure!(
            matches!(
                row.get::<&str, _>("status"),
                "pending" | "enrolled" | "invalid"
            ),
            "credential cannot rotate"
        );
        let credential_id: String = row.get("id");
        let old: i64 = row.get("current_generation");
        let next = old
            .checked_add(1)
            .context("credential generation exhausted")?;
        let backend: String = sqlx::query_scalar("SELECT backend FROM orbit_credential_generations WHERE credential_id=$1 AND generation=$2")
            .bind(&credential_id).bind(old).fetch_one(&mut *tx).await?;
        sqlx::query("UPDATE orbit_credential_generations SET state='retired', retired_at=clock_timestamp() WHERE credential_id=$1 AND generation=$2")
            .bind(&credential_id).bind(old).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO orbit_credential_generations(credential_id,generation,backend,state) VALUES($1,$2,$3,'pending')")
            .bind(&credential_id).bind(next).bind(backend).execute(&mut *tx).await?;
        sqlx::query("UPDATE orbit_credentials SET current_generation=$2,status='pending',updated_at=clock_timestamp() WHERE id=$1")
            .bind(&credential_id).bind(next).execute(&mut *tx).await?;
        tx.commit().await?;
        self.get(reference)
            .await?
            .context("rotated credential missing")
    }

    /// Logical deletion is a durable tombstone. Secret bytes remain retained
    /// and inactive until a separately authorized cleanup policy exists.
    pub async fn revoke(&self, reference: &str) -> Result<Credential> {
        ensure!(logical(reference), "invalid credential reference");
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT id, status FROM orbit_credentials WHERE scope_key='operator' AND reference=$1 FOR UPDATE")
            .bind(reference).fetch_one(&mut *tx).await?;
        let credential_id: String = row.get("id");
        if row.get::<&str, _>("status") != "revoked" {
            sqlx::query("UPDATE orbit_credentials SET status='revoked',updated_at=clock_timestamp() WHERE id=$1")
                .bind(&credential_id).execute(&mut *tx).await?;
            sqlx::query("UPDATE orbit_credential_generations SET state='revoked',retired_at=COALESCE(retired_at,clock_timestamp()) WHERE credential_id=$1")
                .bind(&credential_id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        self.get(reference)
            .await?
            .context("revoked credential missing")
    }

    /// Logical deletion only. A separate, explicitly authorized retention
    /// policy must remove private files; callers cannot reuse the reference.
    pub async fn delete(&self, reference: &str) -> Result<Credential> {
        self.revoke(reference).await
    }
}
