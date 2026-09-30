//! Trusted, operator-provisioned single-call agent adapter. No provider selection.
use crate::{
    agent::{AgentReport, Binding, CallReservation},
    governance::SecretRef,
    model::*,
    worker::Client,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandAgent {
    pub binding_name: String,
    pub binding: Binding,
    pub command: CommandSpec,
    pub tokens_per_call: u64,
    pub cost_microusd_per_call: u64,
    #[serde(default)]
    pub environment: BTreeMap<String, SecretRef>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandOutput {
    pub output: Value,
    #[serde(default)]
    pub delegation_inputs: Vec<String>,
}
impl CommandAgent {
    pub fn validate(&self) -> Result<()> {
        self.binding.validate()?;
        ensure!(
            self.binding.acp.is_none(),
            "command runtime cannot execute ACP bindings"
        );
        self.command.validate()?;
        ensure!(
            crate::agent::valid_name(&self.binding_name),
            "invalid runtime binding name"
        );
        ensure!(
            Path::new(&self.command.argv[0]).is_absolute(),
            "agent executable must be operator-pinned absolute path"
        );
        ensure!(
            self.binding.tools.is_empty(),
            "single-call command runtime does not support tool bindings"
        );
        ensure!(
            self.tokens_per_call > 0
                && Some(self.tokens_per_call) <= self.binding.max_budget.tokens
                && Some(self.cost_microusd_per_call) <= self.binding.max_budget.cost_microusd,
            "runtime reservation exceeds binding budget"
        );
        ensure!(
            self.environment.len() <= 16
                && self.environment.keys().all(|key| {
                    !key.is_empty()
                        && key
                            .bytes()
                            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
                        && !key.starts_with("ORBIT_")
                        && ![
                            "PATH",
                            "HOME",
                            "LD_PRELOAD",
                            "LD_LIBRARY_PATH",
                            "PYTHONPATH",
                            "PYTHONHOME",
                            "NODE_OPTIONS",
                            "BASH_ENV",
                            "ENV",
                        ]
                        .contains(&key.as_str())
                }),
            "invalid runtime credential environment mapping"
        );
        Ok(())
    }
    pub fn authorize(&self, a: &Assignment) -> Result<()> {
        self.validate()?;
        let step = &a.plan.definition.steps[&a.step];
        let spec = step.agent.as_ref().context("agent spec missing")?;
        let digest = digest(&serde_json::to_vec(&self.binding)?);
        ensure!(
            spec.binding == self.binding_name && a.agent_binding_digest.as_deref() == Some(&digest),
            "worker runtime does not match pinned agent binding"
        );
        ensure!(
            spec.tools.is_empty(),
            "single-call command runtime cannot invoke tools"
        );
        spec.authorize(&self.binding)
    }
    pub async fn perform(
        &self,
        client: &Client,
        a: &Assignment,
        directory: &Path,
        home: &Path,
    ) -> Result<(bool, Vec<String>, Option<Failure>)> {
        self.authorize(a)?;
        let spec = a.plan.definition.steps[&a.step].agent.as_ref().unwrap();
        let mut environment = self
            .environment
            .iter()
            .map(|(key, secret)| Ok((key.clone(), secret.resolve()?)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        let input = directory.join("agent-input.json");
        tokio::fs::write(&input, serde_json::to_vec(&json!({
            "format":"orbit-command-agent/v1","task":a.plan.definition.inputs.task,
            "agent":spec,"binding":self.binding,"run_id":a.run_id,"attempt_id":a.attempt_id,
            "generation":a.generation,"idempotency_key":a.idempotency_key,
            "reservation":{"tokens":self.tokens_per_call,"cost_microusd":self.cost_microusd_per_call}
        }))?).await?;
        environment.insert("ORBIT_AGENT_INPUT".into(), input.to_string_lossy().into());
        let reservation = client
            .operation(
                a,
                Action::ReserveAgentCall {
                    reservation: CallReservation {
                        call_id: a.attempt_id.clone(),
                        tokens: Some(self.tokens_per_call),
                        cost_microusd: Some(self.cost_microusd_per_call),
                        tool: None,
                        permissions: spec.permissions.clone(),
                        request_digest: None,
                        acp_charge: None,
                    },
                },
            )
            .await?;
        ensure!(
            reservation["replayed"] == false,
            "reservation replay cannot authorize another dispatch"
        );
        let (code, stdout, stderr, timed_out) =
            crate::worker::agent_command(&self.command, directory, home, a, &environment).await?;
        // Stdout is a typed response, not the log stream. Provider/command stderr
        // can contain sensitive user data and needs review before sharing artifacts.
        let logs = client.upload(a, "logs", stderr).await?;
        if code != Some(0) || timed_out {
            return Ok((
                false,
                vec![logs],
                Some(Failure {
                    category: "infrastructure_failure".into(),
                    code: "agent_command_failed".into(),
                    message: "agent command failed or timed out; external outcome requires review"
                        .into(),
                    side_effect_status: "unknown".into(),
                }),
            ));
        }
        ensure!(
            stdout.len() <= 1024 * 1024,
            "agent command response exceeds 1 MiB"
        );
        let output: CommandOutput =
            serde_json::from_slice(&stdout).context("invalid agent command JSON response")?;
        let report = AgentReport {
            attempt_id: a.attempt_id.clone(),
            binding_digest: a.agent_binding_digest.clone().unwrap(),
            output: output.output,
            delegation_inputs: output.delegation_inputs,
        };
        spec.validate_report(&report, &a.attempt_id, &self.binding)?;
        let artifact = client
            .upload(a, "agent_report", serde_json::to_vec(&report)?)
            .await?;
        Ok((true, vec![logs, artifact], None))
    }
}
