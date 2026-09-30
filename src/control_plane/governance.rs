//! Resource/action authorization and server-owned environment policy.
use crate::{
    agent::{Budget, valid_name},
    compute::Resources,
    model::Definition,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub organization_id: String,
    pub project_id: String,
    pub environment_id: String,
}
impl Scope {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            [
                &self.organization_id,
                &self.project_id,
                &self.environment_id
            ]
            .into_iter()
            .all(|s| valid_name(s) && ![".", ".."].contains(&s.as_str())),
            "invalid execution scope"
        );
        Ok(())
    }
    pub fn parse(value: &str) -> Result<Self> {
        let parts: Vec<_> = value.split('/').collect();
        ensure!(
            parts.len() == 3,
            "scope must be organization/project/environment"
        );
        let scope = Self {
            organization_id: parts[0].into(),
            project_id: parts[1].into(),
            environment_id: parts[2].into(),
        };
        scope.validate()?;
        Ok(scope)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case", deny_unknown_fields)]
pub enum SecretRef {
    Env { name: String },
    File { path: PathBuf },
}
impl SecretRef {
    pub fn resolve(&self) -> Result<String> {
        let value = match self {
            Self::Env { name } => {
                ensure!(
                    !name.is_empty()
                        && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'),
                    "invalid secret environment name"
                );
                std::env::var(name)
                    .context("configured credential environment variable unavailable")?
            }
            Self::File { path } => {
                use std::{
                    io::Read,
                    os::unix::fs::{OpenOptionsExt, PermissionsExt},
                };
                ensure!(path.is_absolute(), "secret file path must be absolute");
                let mut file = std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(path)
                    .context("configured credential file unavailable")?;
                let metadata = file.metadata()?;
                ensure!(
                    metadata.is_file()
                        && metadata.permissions().mode() & 0o077 == 0
                        && metadata.len() <= 8192,
                    "credential file must be a private regular file of at most 8 KiB"
                );
                let mut value = String::new();
                (&mut file).take(8193).read_to_string(&mut value)?;
                ensure!(value.len() <= 8192, "credential file too large");
                value.trim_end_matches(['\r', '\n']).into()
            }
        };
        ensure!(
            (24..=8192).contains(&value.len()) && !value.contains(['\r', '\n']),
            "invalid credential length or characters"
        );
        Ok(value)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalKind {
    User,
    ServiceAccount,
    Agent,
    Integration,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    /// None is an explicit global grant. Scoped grants never grant global operations.
    pub scope: Option<Scope>,
    pub roles: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Principal {
    pub kind: PrincipalKind,
    pub credential: SecretRef,
    pub grants: Vec<Grant>,
    #[serde(skip)]
    pub(crate) token: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub capabilities: Vec<String>,
    pub repository_ids: Vec<String>,
    pub agent_bindings: Vec<String>,
    pub max_resources: Resources,
    pub max_agent_budget: Option<Budget>,
    pub max_concurrency: u32,
}
impl Policy {
    pub fn validate_definition(&self, definition: &Definition) -> Result<()> {
        let mut definitions = vec![definition];
        while let Some(definition) = definitions.pop() {
            ensure!(
                definition.inputs.repository_id.is_empty()
                    || self
                        .repository_ids
                        .contains(&definition.inputs.repository_id),
                "environment policy denies repository"
            );
            ensure!(
                definition.max_concurrency.unwrap_or(8) <= self.max_concurrency,
                "environment policy denies concurrency"
            );
            for step in definition.steps.values() {
                ensure!(
                    self.capabilities.contains(&step.uses),
                    "environment policy denies capability"
                );
                ensure!(
                    step.resources
                        .as_ref()
                        .is_none_or(|r| r.fits(&Resources::default(), &self.max_resources)),
                    "environment policy denies resources"
                );
                if let Some(agent) = &step.agent {
                    ensure!(
                        self.agent_bindings.contains(&agent.binding)
                            && self
                                .max_agent_budget
                                .as_ref()
                                .is_some_and(|budget| agent.budget.fits(budget)),
                        "environment policy denies agent binding/budget"
                    );
                }
                if let Some(child) = &step.definition {
                    definitions.push(child);
                }
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub organization_id: String,
    pub id: String,
    pub environments: BTreeMap<String, Policy>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Governance {
    pub organizations: Vec<String>,
    pub projects: Vec<Project>,
    pub roles: BTreeMap<String, Vec<String>>,
    pub principals: BTreeMap<String, Principal>,
    pub default_scope: Option<Scope>,
}
pub const ACTIONS: &[&str] = &[
    "definition.read",
    "definition.validate",
    "run.read",
    "run.submit",
    "run.cancel",
    "run.signal",
    "run.approve",
    "artifact.read",
    "worker.read",
    "worker.write",
    "system.read",
    "queue.read",
    "limits.read",
    "limits.write",
    "audit.read",
    "package.read",
    "package.publish",
];
impl Governance {
    pub fn resolve(&mut self, tokens: &mut BTreeSet<String>) -> Result<()> {
        ensure!(
            !self.organizations.is_empty()
                && self.organizations.iter().all(|o| valid_name(o))
                && self.organizations.iter().collect::<BTreeSet<_>>().len()
                    == self.organizations.len(),
            "invalid organizations"
        );
        let mut projects = BTreeSet::new();
        for project in &self.projects {
            ensure!(
                self.organizations.contains(&project.organization_id)
                    && valid_name(&project.id)
                    && projects.insert((&project.organization_id, &project.id)),
                "invalid or duplicate project"
            );
            for (name, policy) in &project.environments {
                ensure!(
                    valid_name(name) && (1..=256).contains(&policy.max_concurrency),
                    "invalid environment policy"
                );
                policy.max_resources.validate()?;
                if let Some(budget) = &policy.max_agent_budget {
                    if budget.tokens.is_none() && budget.cost_microusd.is_none() {
                        budget.validate_execution_only()?;
                    } else {
                        budget.validate()?;
                    }
                }
            }
        }
        for (name, actions) in &self.roles {
            ensure!(
                valid_name(name) && actions.iter().all(|a| ACTIONS.contains(&a.as_str())),
                "invalid role or action"
            );
        }
        for (name, principal) in &self.principals {
            ensure!(
                valid_name(name) && name != "operator" && name != "anonymous",
                "invalid principal identity"
            );
            for grant in &principal.grants {
                ensure!(
                    !grant.roles.is_empty()
                        && grant.roles.iter().all(|r| self.roles.contains_key(r)),
                    "unknown role"
                );
                if let Some(scope) = &grant.scope {
                    self.policy(scope)?;
                }
            }
        }
        for principal in self.principals.values_mut() {
            principal.token = principal.credential.resolve()?;
            ensure!(
                tokens.insert(principal.token.clone()),
                "duplicate principal credential"
            );
        }
        if let Some(scope) = &self.default_scope {
            self.policy(scope)?;
        }
        Ok(())
    }
    pub fn policy(&self, scope: &Scope) -> Result<&Policy> {
        scope.validate()?;
        self.projects
            .iter()
            .find(|p| p.organization_id == scope.organization_id && p.id == scope.project_id)
            .and_then(|p| p.environments.get(&scope.environment_id))
            .context("unknown execution environment")
    }
    pub fn authenticate(&self, token: &str) -> Option<(&str, &Principal)> {
        self.principals
            .iter()
            .find(|(_, p)| p.token == token)
            .map(|(name, p)| (name.as_str(), p))
    }
    pub fn allows(&self, principal: &Principal, action: &str, scope: Option<&Scope>) -> bool {
        principal.grants.iter().any(|grant| {
            (grant.scope.is_none() || grant.scope.as_ref() == scope)
                && grant.roles.iter().any(|r| {
                    self.roles
                        .get(r)
                        .is_some_and(|actions| actions.iter().any(|a| a == action))
                })
        })
    }
}
