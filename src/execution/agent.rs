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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_microusd: Option<u64>,
    pub calls: u32,
}
impl Budget {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.tokens.is_some_and(|n| n > 0 && n <= 1_000_000_000),
            "invalid token budget"
        );
        ensure!(
            self.cost_microusd.is_some_and(|n| n <= 1_000_000_000_000)
                && (1..=10000).contains(&self.calls),
            "invalid cost/call budget"
        );
        Ok(())
    }
    pub fn validate_execution_only(&self) -> Result<()> {
        ensure!(
            self.tokens.is_none()
                && self.cost_microusd.is_none()
                && (1..=10000).contains(&self.calls),
            "ACP requires execution-only call accounting"
        );
        Ok(())
    }
    pub fn fits(&self, limit: &Self) -> bool {
        self.tokens.is_some() == limit.tokens.is_some()
            && self.cost_microusd.is_some() == limit.cost_microusd.is_some()
            && self.tokens <= limit.tokens
            && self.cost_microusd <= limit.cost_microusd
            && self.calls <= limit.calls
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    /// Immutable deployment/model revision, not a credential or mutable alias.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// An additional server-authorized worker capability.
    pub runtime: String,
    /// Tool name -> immutable tool implementation revision.
    pub tools: BTreeMap<String, String>,
    pub permissions: Vec<String>,
    pub max_budget: Budget,
    pub max_delegations: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp: Option<crate::acp_contract::Descriptor>,
}
impl Binding {
    pub fn validate(&self) -> Result<()> {
        if self
            .acp
            .as_ref()
            .is_some_and(|a| a.model_policy == crate::acp_contract::ModelPolicy::AgentConfigured)
        {
            ensure!(
                self.model.is_none(),
                "agent-configured ACP model must be omitted"
            );
        } else {
            ensure!(
                self.model
                    .as_ref()
                    .is_some_and(|s| !s.trim().is_empty() && s.len() <= 512),
                "model revision required"
            );
        }
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
        if let Some(acp) = &self.acp {
            acp.validate()?;
            ensure!(self.max_delegations == 0, "ACP delegation is unsupported");
            for (name, revision) in &self.tools {
                ensure!(
                    crate::coding_agent::tool_permissions(name).is_some()
                        && revision == &format!("orbit.acp.workspace.{name}/v1"),
                    "unsupported ACP broker tool revision"
                );
            }
            self.max_budget.validate_execution_only()
        } else {
            self.max_budget.validate()
        }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_limits: Option<crate::acp_contract::Limits>,
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
        if let Some(limits) = &self.acp_limits {
            limits.validate()?;
            ensure!(self.max_delegations == 0, "ACP delegation is unsupported");
            self.budget.validate_execution_only()
        } else {
            self.budget.validate()
        }
    }
    pub fn authorize(&self, binding: &Binding) -> Result<()> {
        self.validate()?;
        binding.validate()?;
        match (&self.acp_limits, &binding.acp) {
            (Some(limits), Some(acp)) => {
                ensure!(limits.fits(&acp.max_limits), "ACP limits exceed binding");
                for name in &self.tools {
                    ensure!(
                        crate::coding_agent::tool_permissions(name).is_some_and(|required| {
                            required
                                .iter()
                                .all(|p| self.permissions.iter().any(|granted| granted == p))
                        }),
                        "ACP tool permission missing"
                    );
                }
            }
            (None, None) => {}
            _ => anyhow::bail!("ACP definition/binding mismatch"),
        }
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
        if let Some(acp) = &binding.acp {
            let metadata = &report.output["acp"];
            ensure!(
                metadata["launch_digest"] == acp.launch_digest
                    && metadata["model"] == serde_json::to_value(&binding.model)?
                    && metadata["stop_reason"] == "end_turn"
                    && metadata["accounting"] == "execution_only"
                    && metadata.get("tokens") == Some(&Value::Null)
                    && metadata.get("cost_microusd") == Some(&Value::Null)
                    && metadata["model_attribution"]
                        == if binding.model.is_some() {
                            "agent_confirmed_exact"
                        } else {
                            "agent_configured_unverified"
                        },
                "ACP report policy or attribution mismatch"
            );
        }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_microusd: Option<u64>,
    /// None is a model invocation. Tool calls must name an allowed bound tool.
    pub tool: Option<String>,
    pub permissions: Vec<String>,
    /// Tracked dispatch intent for the coding runtime; absent in legacy reservations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_charge: Option<crate::acp_contract::Charge>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallReceipt {
    pub call_id: String,
    pub attempt_id: String,
    pub result_digest: String,
    pub external_id: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Usage {
    pub tokens: Option<u64>,
    pub cost_microusd: Option<u64>,
    pub reservations: BTreeMap<String, CallReservation>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub receipts: BTreeMap<String, CallReceipt>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub acp_sessions: BTreeMap<String, crate::acp_contract::SessionUsage>,
}
impl Default for Usage {
    fn default() -> Self {
        Self {
            tokens: Some(0),
            cost_microusd: Some(0),
            reservations: BTreeMap::new(),
            receipts: BTreeMap::new(),
            acp_sessions: BTreeMap::new(),
        }
    }
}
impl Usage {
    pub fn reserve(&mut self, spec: &AgentSpec, call: &CallReservation) -> Result<bool> {
        spec.validate()?;
        ensure!(
            call.request_digest.as_deref().is_none_or(valid_digest),
            "invalid invocation request digest"
        );
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
        let (tokens, cost) = if let Some(limits) = &spec.acp_limits {
            self.validate_acp_charge(limits, call)?;
            (None, None)
        } else {
            ensure!(call.acp_charge.is_none(), "ACP charge on legacy invocation");
            let add = |current: Option<u64>, charge: Option<u64>| -> Result<Option<u64>> {
                Ok(Some(
                    current
                        .zip(charge)
                        .and_then(|(a, b)| a.checked_add(b))
                        .ok_or_else(|| anyhow::anyhow!("missing accounting or budget overflow"))?,
                ))
            };
            (
                add(self.tokens, call.tokens)?,
                add(self.cost_microusd, call.cost_microusd)?,
            )
        };
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
    fn validate_acp_charge(
        &self,
        limits: &crate::acp_contract::Limits,
        call: &CallReservation,
    ) -> Result<()> {
        use crate::acp_contract::Charge;
        ensure!(
            call.tokens.is_none() && call.cost_microusd.is_none() && call.request_digest.is_some(),
            "ACP requires tracked execution-only reservations"
        );
        let charge = call
            .acp_charge
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("ACP charge missing"))?;
        match charge {
            Charge::Prompt => ensure!(
                call.tool.is_none() && call.permissions.is_empty(),
                "invalid ACP prompt charge"
            ),
            Charge::Broker {
                terminal_runtime_seconds,
            } => {
                ensure!(call.tool.is_some(), "ACP broker charge requires a tool");
                ensure!(
                    crate::coding_agent::tool_permissions(call.tool.as_deref().unwrap())
                        .is_some_and(|required| required
                            .iter()
                            .all(|p| call.permissions.iter().any(|granted| granted == p))),
                    "ACP broker permission missing"
                );
                if call.tool.as_deref() == Some("shell") {
                    ensure!(
                        (1..=limits.terminal_timeout_seconds).contains(terminal_runtime_seconds),
                        "ACP terminal reservation invalid"
                    );
                } else {
                    ensure!(
                        *terminal_runtime_seconds == 0,
                        "non-terminal runtime charge"
                    );
                }
            }
        }
        let mut prompts = 0u64;
        let mut brokers = 0u64;
        let mut seconds = 0u64;
        for charge in self
            .reservations
            .values()
            .map(|r| r.acp_charge.as_ref())
            .chain(std::iter::once(Some(charge)))
        {
            match charge {
                Some(Charge::Prompt) => prompts += 1,
                Some(Charge::Broker {
                    terminal_runtime_seconds,
                }) => {
                    brokers += 1;
                    seconds = seconds
                        .checked_add(*terminal_runtime_seconds)
                        .ok_or_else(|| anyhow::anyhow!("ACP runtime overflow"))?;
                }
                None => anyhow::bail!("mixed invocation accounting"),
            }
        }
        ensure!(
            prompts <= u64::from(limits.prompt_turns)
                && brokers <= u64::from(limits.broker_calls)
                && seconds <= limits.terminal_runtime_seconds,
            "ACP execution budget exhausted"
        );
        Ok(())
    }
    pub fn finish(&mut self, receipt: &CallReceipt) -> Result<bool> {
        ensure!(
            self.reservations
                .get(&receipt.call_id)
                .is_some_and(|r| r.request_digest.is_some()),
            "tracked invocation reservation missing"
        );
        ensure!(
            receipt
                .call_id
                .starts_with(&format!("{}-", receipt.attempt_id))
                && valid_digest(&receipt.result_digest)
                && receipt
                    .external_id
                    .as_deref()
                    .is_none_or(|id| !id.is_empty()
                        && id.len() <= 256
                        && id
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))),
            "invalid invocation receipt"
        );
        if let Some(previous) = self.receipts.get(&receipt.call_id) {
            ensure!(previous == receipt, "conflict: invocation receipt changed");
            return Ok(false);
        }
        self.receipts
            .insert(receipt.call_id.clone(), receipt.clone());
        Ok(true)
    }
    pub fn pending_model_call(&self) -> bool {
        self.reservations.values().any(|r| {
            r.request_digest.is_some()
                && r.tool.is_none()
                && !self.receipts.contains_key(&r.call_id)
        })
    }
    pub fn pending_attempt_call(&self, attempt_id: &str) -> bool {
        self.reservations.values().any(|r| {
            r.request_digest.is_some()
                && r.call_id.starts_with(&format!("{attempt_id}-"))
                && !self.receipts.contains_key(&r.call_id)
        })
    }
}

pub(crate) fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|c| c.is_ascii_hexdigit())
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
