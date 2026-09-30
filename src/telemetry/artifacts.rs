//! Immutable artifact publication. Storage I/O must never hold engine database locks.
use crate::model::{Artifact, digest, id};
use anyhow::{Context, Result, ensure};
use object_store::{
    ObjectStore, PutMode, PutOptions, aws::AmazonS3Builder, path::Path as ObjectPath,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub const MAX_ARTIFACT_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct S3Config {
    pub bucket: String,
    pub region: String,
    pub endpoint: Option<String>,
    pub access_key_env: String,
    pub secret_key_env: String,
    #[serde(default)]
    pub allow_http: bool,
    #[serde(default)]
    pub prefix: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactLocation {
    pub provider: String,
    pub object_key: String,
    pub content_type: String,
}

#[derive(Clone)]
pub struct ArtifactStores {
    root: PathBuf,
    remote: BTreeMap<String, (Arc<dyn ObjectStore>, String)>,
    default: String,
}
impl ArtifactStores {
    pub fn local(root: PathBuf) -> Self {
        Self {
            root,
            remote: BTreeMap::new(),
            default: "local".into(),
        }
    }
    /// Install an operator-selected ObjectStore adapter. Useful for embedded
    /// deployments and controlled storage fault injection; never from a Definition.
    pub fn with_provider(mut self, name: &str, provider: Arc<dyn ObjectStore>) -> Result<Self> {
        ensure!(
            name != "local" && !name.is_empty() && !self.remote.contains_key(name),
            "invalid or duplicate artifact provider"
        );
        self.remote.insert(name.into(), (provider, String::new()));
        self.default = name.into();
        Ok(self)
    }
    pub fn configure(
        mut self,
        providers: &BTreeMap<String, S3Config>,
        default: Option<&str>,
    ) -> Result<Self> {
        for (name, config) in providers {
            ensure!(
                name != "local" && !name.is_empty() && name.len() <= 128,
                "invalid artifact provider name"
            );
            ensure!(
                !config.bucket.is_empty() && !config.region.is_empty(),
                "S3 bucket and region required"
            );
            ensure!(
                config.prefix.is_empty()
                    || config.prefix.split('/').all(|p| !p.is_empty()
                        && p != "."
                        && p != ".."
                        && p.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))),
                "invalid S3 key prefix"
            );
            let access = std::env::var(&config.access_key_env)
                .context("S3 access key environment variable missing")?;
            let secret = std::env::var(&config.secret_key_env)
                .context("S3 secret key environment variable missing")?;
            let mut builder = AmazonS3Builder::new()
                .with_bucket_name(&config.bucket)
                .with_region(&config.region)
                .with_access_key_id(access)
                .with_secret_access_key(secret)
                .with_client_options(
                    object_store::ClientOptions::new()
                        .with_allow_http(config.allow_http)
                        .with_timeout(Duration::from_secs(20)),
                );
            if let Some(endpoint) = &config.endpoint {
                let url = reqwest::Url::parse(endpoint)?;
                ensure!(
                    url.username().is_empty()
                        && url.password().is_none()
                        && url.query().is_none()
                        && url.fragment().is_none(),
                    "S3 endpoint must not contain credentials, query or fragment"
                );
                builder = builder.with_endpoint(endpoint);
            }
            self.remote.insert(
                name.clone(),
                (Arc::new(builder.build()?), config.prefix.clone()),
            );
        }
        if let Some(default) = default {
            ensure!(
                default == "local" || self.remote.contains_key(default),
                "default artifact provider not configured"
            );
            self.default = default.into();
        }
        Ok(self)
    }
    pub fn location(&self, artifact_id: &str, kind: &str) -> ArtifactLocation {
        let prefix = self
            .remote
            .get(&self.default)
            .map(|(_, p)| p.as_str())
            .unwrap_or("");
        ArtifactLocation {
            provider: self.default.clone(),
            object_key: if prefix.is_empty() {
                artifact_id.into()
            } else {
                format!("{prefix}/{artifact_id}")
            },
            content_type: if matches!(kind, "manifest" | "test_report" | "container_report") {
                "application/json"
            } else {
                "application/octet-stream"
            }
            .into(),
        }
    }
    pub fn local_path(&self, artifact_id: &str) -> Result<PathBuf> {
        uuid::Uuid::parse_str(artifact_id)?;
        Ok(self.root.join(artifact_id))
    }
    fn remote(&self, artifact: &Artifact) -> Result<Option<(&Arc<dyn ObjectStore>, ObjectPath)>> {
        let Some(location) = &artifact.location else {
            return Ok(None);
        };
        if location.provider == "local" {
            ensure!(
                location.object_key == artifact.id,
                "invalid local object key"
            );
            return Ok(None);
        }
        let (store, _) = self
            .remote
            .get(&location.provider)
            .context("artifact provider unavailable")?;
        Ok(Some((store, ObjectPath::parse(&location.object_key)?)))
    }
    pub async fn read(&self, artifact: &Artifact) -> Result<Vec<u8>> {
        ensure!(artifact.size <= MAX_ARTIFACT_BYTES, "artifact too large");
        let bytes = if let Some((store, key)) = self.remote(artifact)? {
            let result = store
                .get(&key)
                .await
                .map_err(|_| anyhow::anyhow!("artifact provider read failed"))?;
            ensure!(result.meta.size == artifact.size, "artifact size mismatch");
            result
                .bytes()
                .await
                .map_err(|_| anyhow::anyhow!("artifact provider read failed"))?
                .to_vec()
        } else {
            let path = self.local_path(&artifact.id)?;
            ensure!(
                tokio::fs::metadata(&path).await?.len() == artifact.size,
                "artifact size mismatch"
            );
            tokio::fs::read(path)
                .await
                .context("artifact bytes missing")?
        };
        ensure!(
            bytes.len() as u64 == artifact.size && digest(&bytes) == artifact.checksum,
            "artifact checksum mismatch"
        );
        Ok(bytes)
    }
    pub async fn publish(&self, artifact: &Artifact, bytes: &[u8]) -> Result<()> {
        ensure!(
            bytes.len() as u64 == artifact.size
                && artifact.size <= MAX_ARTIFACT_BYTES
                && digest(bytes) == artifact.checksum,
            "artifact content mismatch"
        );
        if let Some((store, key)) = self.remote(artifact)? {
            match store
                .put_opts(
                    &key,
                    bytes.to_vec().into(),
                    PutOptions {
                        mode: PutMode::Create,
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(_) => (),
                Err(_) => {
                    // Conditional conflicts and lost PUT responses have the same
                    // reconciliation rule: accept only the already-published,
                    // checksum-identical object. Never fall back to overwrite.
                    self.read(artifact)
                        .await
                        .context("artifact provider publication failed")?;
                }
            }
        } else {
            let path = self.local_path(&artifact.id)?;
            let bytes = bytes.to_vec();
            tokio::task::spawn_blocking(move || publish_local(&path, &bytes)).await??;
        }
        Ok(())
    }
}

fn publish_local(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    if path.exists() {
        ensure!(std::fs::read(path)? == bytes, "immutable artifact conflict");
        std::fs::File::open(path)?.sync_all()?;
        std::fs::File::open(path.parent().unwrap())?.sync_all()?;
        return Ok(());
    }
    let tmp = path.with_extension(format!("{}.upload", id()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        match std::fs::hard_link(&tmp, path) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                ensure!(std::fs::read(path)? == bytes, "immutable artifact conflict")
            }
            Err(e) => return Err(e.into()),
        }
        std::fs::File::open(path.parent().unwrap())?.sync_all()?;
        Ok(())
    })();
    let _ = std::fs::remove_file(tmp);
    result
}
