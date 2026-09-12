//! Pinned agent contracts. Provider credentials and reasoning stay in trusted runtimes.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

fn names(values: &[String]) -> bool {
    values.len() <= 64
        && values.iter().collect::<BTreeSet<_>>().len() == values.len()
        && values.iter().all(|s| valid_name(s))
}
pub fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[derive(schemars::JsonSchema)]
pub struct Budget {
    pub tokens: u64,
    pub cost_microusd: u64,
    pub calls: u32,
}
impl Budget {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.tokens > 0 && self.tokens <= 1_000_000_000,
            "invalid token budget"
        );
        ensure!(
            self.cost_microusd <= 1_000_000_000_000 && (1..=10000).contains(&self.calls),
            "invalid cost/call budget"
        );
        Ok(())
    }
    pub fn fits(&self, limit: &Self) -> bool {
        self.tokens <= limit.tokens
            && self.cost_microusd <= limit.cost_microusd
            && self.calls <= limit.calls
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    /// Immutable deployment/model revision, not a credential or mutable alias.
    pub model: String,
    /// An additional server-authorized worker capability.
    pub runtime: String,
    /// Tool name -> immutable tool implementation revision.
    pub tools: BTreeMap<String, String>,
    pub permissions: Vec<String>,
    pub max_budget: Budget,
    pub max_delegations: u32,
}
impl Binding {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.model.trim().is_empty() && self.model.len() <= 512,
            "model revision required"
        );
        ensure!(
            valid_name(&self.runtime) && self.runtime != "agent.run",
            "distinct runtime capability required"
        );
        ensure!(
            names(&self.tools.keys().cloned().collect::<Vec<_>>())
                && self
                    .tools
                    .values()
                    .all(|v| !v.trim().is_empty() && v.len() <= 512),
            "invalid tool bindings"
        );
        ensure!(
            names(&self.permissions) && self.max_delegations <= 64,
            "invalid agent permissions/delegation bound"
        );
        self.max_budget.validate()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[derive(schemars::JsonSchema)]
pub struct AgentSpec {
    pub identity: String,
    pub binding: String,
    pub tools: Vec<String>,
    pub permissions: Vec<String>,
    pub budget: Budget,
    #[serde(default)]
    pub max_delegations: u32,
    #[serde(default)]
    pub context: Value,
    pub output: OutputContract,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(schemars::JsonSchema)]
pub enum OutputContract {
    Json,
    Object,
    Array,
    String,
}
impl AgentSpec {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            valid_name(&self.identity) && valid_name(&self.binding),
            "invalid agent identity/binding"
        );
        ensure!(
            names(&self.tools) && names(&self.permissions),
            "invalid requested tools/permissions"
        );
        ensure!(
            self.max_delegations <= 64 && serde_json::to_vec(&self.context)?.len() <= 16384,
            "agent context/delegation limit exceeded"
        );
        self.budget.validate()
    }
    pub fn authorize(&self, binding: &Binding) -> Result<()> {
        binding.validate()?;
        ensure!(
            self.tools.iter().all(|t| binding.tools.contains_key(t))
                && self
                    .permissions
                    .iter()
                    .all(|p| binding.permissions.contains(p)),
            "agent tools or permissions denied by binding"
        );
        ensure!(
            self.budget.fits(&binding.max_budget)
                && self.max_delegations <= binding.max_delegations,
            "agent budget or delegation denied by binding"
        );
        Ok(())
    }
    pub fn validate_report(
        &self,
        report: &AgentReport,
        attempt: &str,
        binding: &Binding,
    ) -> Result<()> {
        ensure!(
            report.attempt_id == attempt
                && report.binding_digest == crate::model::digest(&serde_json::to_vec(binding)?),
            "agent report provenance mismatch"
        );
        ensure!(
            match self.output {
                OutputContract::Json => true,
                OutputContract::Object => report.output.is_object(),
                OutputContract::Array => report.output.is_array(),
                OutputContract::String => report.output.is_string(),
            },
            "agent output contract mismatch"
        );
        ensure!(
            serde_json::to_vec(&report.output)?.len() <= 65536
                && report.delegation_inputs.len() <= self.max_delegations as usize
                && report
                    .delegation_inputs
                    .iter()
                    .all(|s| !s.trim().is_empty() && s.len() <= 16384),
            "agent output or delegation limit exceeded"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentReport {
    pub attempt_id: String,
    pub binding_digest: String,
    pub output: Value,
    #[serde(default)]
    pub delegation_inputs: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CallReservation {
    pub call_id: String,
    pub tokens: u64,
    pub cost_microusd: u64,
    /// None is a model invocation. Tool calls must name an allowed bound tool.
    pub tool: Option<String>,
    pub permissions: Vec<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    pub tokens: u64,
    pub cost_microusd: u64,
    pub reservations: BTreeMap<String, CallReservation>,
}
impl Usage {
    pub fn reserve(&mut self, spec: &AgentSpec, call: &CallReservation) -> Result<bool> {
        ensure!(
            valid_name(&call.call_id) && names(&call.permissions),
            "invalid call reservation"
        );
        ensure!(
            call.tool.as_ref().is_none_or(|t| spec.tools.contains(t))
                && call
                    .permissions
                    .iter()
                    .all(|p| spec.permissions.contains(p)),
            "agent call permission denied"
        );
        if let Some(previous) = self.reservations.get(&call.call_id) {
            ensure!(
                previous == call,
                "conflict: call ID reused with different reservation"
            );
            return Ok(false);
        }
        let tokens = self
            .tokens
            .checked_add(call.tokens)
            .ok_or_else(|| anyhow::anyhow!("token budget overflow"))?;
        let cost = self
            .cost_microusd
            .checked_add(call.cost_microusd)
            .ok_or_else(|| anyhow::anyhow!("cost budget overflow"))?;
        ensure!(
            tokens <= spec.budget.tokens
                && cost <= spec.budget.cost_microusd
                && self.reservations.len() < spec.budget.calls as usize,
            "agent budget exhausted"
        );
        self.tokens = tokens;
        self.cost_microusd = cost;
        self.reservations.insert(call.call_id.clone(), call.clone());
        Ok(true)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[derive(schemars::JsonSchema)]
pub struct ApprovalSpec {
    pub assignees: Vec<String>,
    pub prompt: String,
}
impl ApprovalSpec {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.assignees.is_empty() && names(&self.assignees),
            "approval assignees required"
        );
        ensure!(
            !self.prompt.trim().is_empty() && self.prompt.len() <= 4096,
            "approval prompt required (max 4096 bytes)"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Approval {
    pub request_id: String,
    pub step: String,
    pub approved: bool,
    pub comment: String,
}
