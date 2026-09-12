//! Portable execution requirements and the first operator-selected OCI profile.
use crate::{agent::valid_name, compute::ContainerSpec, model::CommandSpec};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub const CAPABILITY: &str = "execution.podman-v1";

#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    Trusted,
    Sandboxed,
    Untrusted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Network {
    None,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Filesystem {
    Workspace,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Requirements {
    pub isolation: Isolation,
    pub network: Network,
    pub filesystem: Filesystem,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    RootlessPodman,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub backend: Backend,
    /// Operator-provisioned image containing the repository's tools, not a host path.
    pub image: String,
}

impl Profile {
    pub fn validate(&self, isolation: &Isolation) -> Result<()> {
        ensure!(
            *isolation == Isolation::Trusted,
            "isolation class has no qualified backend"
        );
        ContainerSpec {
            image: self.image.clone(),
            command: vec!["sh".into()],
        }
        .validate()
    }
}

/// Names rather than secret values enter repository/provider bindings.
pub fn credential_name(name: &str) -> Result<()> {
    ensure!(valid_name(name), "invalid logical credential reference");
    Ok(())
}

pub fn validate_tool_command(command: &CommandSpec) -> Result<()> {
    command.validate()?;
    ensure!(
        command.argv.len() <= 128
            && command
                .argv
                .iter()
                .all(|s| s.len() <= 65536 && !s.contains('\0')),
        "tool command exceeds bounds"
    );
    Ok(())
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerConfig {
    pub profiles: Vec<Profile>,
    pub repository_ids: Vec<String>,
    #[serde(default)]
    pub credentials: std::collections::BTreeMap<String, crate::repository::Credential>,
    #[serde(default)]
    pub coding_agent: Option<crate::coding_agent::Runtime>,
}

impl WorkerConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.profiles.is_empty() && self.profiles.len() <= 16,
            "provision execution profiles"
        );
        for profile in &self.profiles {
            profile.validate(&Isolation::Trusted)?;
        }
        ensure!(
            !self.repository_ids.is_empty()
                && self.repository_ids.len() <= 64
                && self.repository_ids.iter().all(|name| valid_name(name)),
            "invalid repository admission list"
        );
        ensure!(self.credentials.len() <= 64, "too many worker credentials");
        for (name, credential) in &self.credentials {
            credential_name(name)?;
            ensure!(
                valid_name(&credential.binding),
                "invalid credential binding"
            );
            for scope in &credential.scopes {
                scope.validate()?;
            }
        }
        if let Some(agent) = &self.coding_agent {
            agent.validate()?;
        }
        Ok(())
    }
    pub fn authorize(&self, a: &crate::model::Assignment) -> Result<&Profile> {
        self.validate()?;
        ensure!(
            self.repository_ids
                .contains(&a.plan.definition.inputs.repository_id),
            "worker denies repository"
        );
        let requirements = a.plan.definition.steps[&a.step]
            .execution
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("workspace execution requirements missing"))?;
        let selected = a
            .plan
            .execution_profiles
            .get(&requirements.isolation)
            .ok_or_else(|| anyhow::anyhow!("pinned execution profile missing"))?;
        selected.validate(&requirements.isolation)?;
        self.profiles
            .iter()
            .find(|profile| *profile == selected)
            .ok_or_else(|| anyhow::anyhow!("worker denies pinned execution profile"))
    }
}
