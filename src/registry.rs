//! Verified metadata registry. Packages never load code into the server process.
use crate::{
    agent::valid_name,
    engine::Engine,
    governance::Scope,
    model::{Definition, digest},
};
use anyhow::{Context, Result, ensure};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;
use std::collections::BTreeMap;

fn name(value: &str) -> bool {
    valid_name(value) && value != "." && value != ".."
}
fn namespace(value: &str) -> bool {
    let parts: Vec<_> = value.split('/').collect();
    (1..=2).contains(&parts.len()) && parts.iter().all(|p| name(p))
}
fn version(value: &str) -> bool {
    let parts: Vec<_> = value.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| p.parse::<u32>().is_ok_and(|v| v.to_string() == *p))
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityPackage {
    pub protocol_version: String,
    /// An immutable OCI worker image; provisioning is a separate operator action.
    pub worker_image: String,
    pub input_schema: Value,
    pub output_schema: Value,
    #[serde(default)]
    pub ui: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub namespace: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub capabilities: BTreeMap<String, CapabilityPackage>,
    pub definitions: BTreeMap<String, Definition>,
}
impl Manifest {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.api_version == "orbit.package/v1"
                && namespace(&self.namespace)
                && name(&self.name)
                && version(&self.version),
            "invalid package identity/version"
        );
        ensure!(
            self.description.len() <= 2048
                && self.capabilities.len() <= 64
                && self.definitions.len() <= 64
                && (!self.capabilities.is_empty() || !self.definitions.is_empty()),
            "invalid package bounds"
        );
        for (capability, package) in &self.capabilities {
            ensure!(
                name(capability) && package.protocol_version == "orbit/v0",
                "invalid capability package protocol"
            );
            crate::compute::ContainerSpec {
                image: package.worker_image.clone(),
                command: vec!["worker".into()],
            }
            .validate()?;
            ensure!(
                package.input_schema.is_object() && package.output_schema.is_object(),
                "capability schemas must be JSON objects"
            );
        }
        for (name, definition) in &self.definitions {
            ensure!(self::name(name), "invalid packaged definition name");
            definition.validate()?;
        }
        ensure!(
            self.canonical_bytes()?.len() <= 1024 * 1024,
            "package manifest exceeds 1 MiB"
        );
        Ok(())
    }
    /// Format v1 uses compact serde_json Value encoding, recursively sorted object keys.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&serde_json::to_value(self)?)?)
    }
    pub fn digest(&self) -> Result<String> {
        Ok(digest(&self.canonical_bytes()?))
    }
    pub fn signing_message(&self) -> Result<Vec<u8>> {
        Ok(format!("orbit.package/v1\n{}", self.digest()?).into_bytes())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustedPublisher {
    /// Hex-encoded Ed25519 public key, never a signing key.
    pub public_key: String,
    pub namespaces: Vec<String>,
}
impl TrustedPublisher {
    pub fn validate(&self) -> Result<VerifyingKey> {
        ensure!(
            !self.namespaces.is_empty() && self.namespaces.iter().all(|n| namespace(n)),
            "publisher namespaces required"
        );
        let bytes: [u8; 32] = hex::decode(&self.public_key)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("publisher public key must be 32 bytes"))?;
        Ok(VerifyingKey::from_bytes(&bytes)?)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Package {
    pub manifest: Manifest,
    pub digest: String,
    pub key_id: String,
    /// Ed25519 signature of Manifest::signing_message(), encoded as hex.
    pub signature: String,
}
impl Package {
    pub fn verify(&self, publishers: &BTreeMap<String, TrustedPublisher>) -> Result<()> {
        self.manifest.validate()?;
        ensure!(
            self.digest == self.manifest.digest()?,
            "package digest mismatch"
        );
        let publisher = publishers
            .get(&self.key_id)
            .context("untrusted package publisher")?;
        ensure!(
            publisher.namespaces.contains(&self.manifest.namespace),
            "publisher not authorized for namespace"
        );
        let key = publisher.validate()?;
        key.verify_strict(
            &self.manifest.signing_message()?,
            &Signature::from_slice(&hex::decode(&self.signature)?)?,
        )
        .context("package signature verification failed")?;
        Ok(())
    }
}
pub fn scope_key(scope: Option<&Scope>) -> String {
    scope.map_or_else(
        || String::from("legacy"),
        |s| {
            format!(
                "{}/{}/{}",
                s.organization_id, s.project_id, s.environment_id
            )
        },
    )
}
impl Engine {
    pub async fn publish_package(
        &self,
        package: &Package,
        scope: Option<&Scope>,
        actor: &str,
        publishers: &BTreeMap<String, TrustedPublisher>,
    ) -> Result<Value> {
        package.verify(publishers)?;
        if let Some(scope) = scope {
            scope.validate()?;
            ensure!(
                package.manifest.namespace
                    == format!("{}/{}", scope.organization_id, scope.project_id),
                "package namespace must match execution project"
            );
        }
        let mut tx = self.pool.begin().await?;
        // Shares the existing control-row boundary so concurrent replicas cannot replace versions.
        sqlx::query("SELECT id FROM orbit_control WHERE id=1 FOR UPDATE")
            .execute(&mut *tx)
            .await?;
        let key = scope_key(scope);
        let existing: Option<Value> = sqlx::query_scalar("SELECT envelope FROM orbit_packages WHERE scope_key=$1 AND namespace=$2 AND name=$3 AND version=$4").bind(&key).bind(&package.manifest.namespace).bind(&package.manifest.name).bind(&package.manifest.version).fetch_optional(&mut *tx).await?;
        if let Some(existing) = existing {
            ensure!(
                existing == json!(package),
                "conflict: package version and signature envelope are immutable"
            );
            return Ok(json!({"status":"accepted","digest":package.digest,"duplicate":true}));
        }
        sqlx::query("INSERT INTO orbit_packages(scope_key,namespace,name,version,digest,envelope,published_by) VALUES($1,$2,$3,$4,$5,$6,$7)").bind(key).bind(&package.manifest.namespace).bind(&package.manifest.name).bind(&package.manifest.version).bind(&package.digest).bind(json!(package)).bind(actor).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(json!({"status":"accepted","digest":package.digest,"duplicate":false}))
    }
    pub async fn package(
        &self,
        digest: &str,
        scope: Option<&Scope>,
        publishers: &BTreeMap<String, TrustedPublisher>,
    ) -> Result<Value> {
        ensure!(
            digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid package digest"
        );
        let row = sqlx::query("SELECT envelope,published_by,published_at::text AS published_at FROM orbit_packages WHERE scope_key=$1 AND digest=$2").bind(scope_key(scope)).bind(digest).fetch_optional(&self.pool).await?.context("package not found")?;
        let package: Package = serde_json::from_value(row.get("envelope"))?;
        package.verify(publishers)?;
        ensure!(package.digest == digest, "stored package digest mismatch");
        if let Some(scope) = scope {
            ensure!(
                package.manifest.namespace
                    == format!("{}/{}", scope.organization_id, scope.project_id),
                "stored package scope mismatch"
            );
        }
        Ok(
            json!({"package":package,"verified":true,"published_by":row.get::<String,_>("published_by"),"published_at":row.get::<String,_>("published_at")}),
        )
    }
    pub async fn packages(
        &self,
        scope: Option<&Scope>,
        publishers: &BTreeMap<String, TrustedPublisher>,
    ) -> Result<Value> {
        let rows = sqlx::query("SELECT envelope,published_by,published_at::text AS published_at FROM orbit_packages WHERE scope_key=$1 ORDER BY published_at DESC LIMIT 100").bind(scope_key(scope)).fetch_all(&self.pool).await?;
        let mut results = vec![];
        for row in rows {
            let package: Package = serde_json::from_value(row.get("envelope"))?;
            results.push(json!({"namespace":package.manifest.namespace,"name":package.manifest.name,"version":package.manifest.version,"digest":package.digest,"key_id":package.key_id,"verified":package.verify(publishers).is_ok(),"published_by":row.get::<String,_>("published_by"),"published_at":row.get::<String,_>("published_at")}));
        }
        Ok(json!(results))
    }
}
