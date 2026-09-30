//! Portable compute requirements and operator-owned worker capacity.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(schemars::JsonSchema)]
pub struct Resources {
    pub cpu_millis: u32,
    pub memory_mib: u32,
    pub gpu: u32,
}
impl Resources {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.cpu_millis <= 1_024_000 && self.memory_mib <= 4_194_304 && self.gpu <= 64,
            "resource requirements exceed supported bounds"
        );
        Ok(())
    }
    pub fn fits(&self, used: &Self, capacity: &Self) -> bool {
        self.cpu_millis as u64 + used.cpu_millis as u64 <= capacity.cpu_millis as u64
            && self.memory_mib as u64 + used.memory_mib as u64 <= capacity.memory_mib as u64
            && self.gpu as u64 + used.gpu as u64 <= capacity.gpu as u64
    }
    pub fn reserve(&mut self, resources: &Self) {
        self.cpu_millis = self.cpu_millis.saturating_add(resources.cpu_millis);
        self.memory_mib = self.memory_mib.saturating_add(resources.memory_mib);
        self.gpu = self.gpu.saturating_add(resources.gpu);
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(schemars::JsonSchema)]
pub struct Placement {
    pub pool: Option<String>,
    pub capabilities: Vec<String>,
}
impl Placement {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.pool.as_deref().is_none_or(valid_name)
                && self.capabilities.len() <= 32
                && self.capabilities.iter().all(|c| valid_name(c)),
            "invalid worker placement"
        );
        Ok(())
    }
}
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkerCapacity {
    pub pool: Option<String>,
    pub resources: Resources,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[derive(schemars::JsonSchema)]
pub struct ContainerSpec {
    /// Immutable OCI digest. Images must be provisioned on the runner by its operator.
    pub image: String,
    pub command: Vec<String>,
}
impl ContainerSpec {
    pub fn validate(&self) -> Result<()> {
        let (name, checksum) = self.image.rsplit_once("@sha256:").unwrap_or_default();
        ensure!(
            !name.is_empty()
                && !name.starts_with('-')
                && name.len() <= 512
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/._:-".contains(&b))
                && checksum.len() == 64
                && checksum.bytes().all(|b| b.is_ascii_hexdigit()),
            "container image must be pinned by sha256 digest"
        );
        ensure!(
            !self.command.is_empty()
                && self.command.len() <= 128
                && !self.command[0].is_empty()
                && self
                    .command
                    .iter()
                    .all(|s| s.len() <= 16384 && !s.contains('\0')),
            "invalid container command"
        );
        Ok(())
    }
}
