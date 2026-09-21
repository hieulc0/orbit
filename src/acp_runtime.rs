//! Operator-owned ACP installation registry. No executable or auth path enters a plan.
use crate::{
    acp_broker::Broker,
    acp_wire::Wire,
    agent::{Binding, OutputContract, valid_name},
    model::{Assignment, digest},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Component, PathBuf},
};
pub const WORKSPACE: &str = "/orbit/home/workspace";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Adapter {
    Codex,
    Acp,
    Antigravity,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentNetwork {
    None,
    Host,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Launch {
    pub adapter: Adapter,
    pub image: String,
    pub command: Vec<String>,
    pub agent_name: String,
    pub agent_version: String,
    pub binary_revision: String,
    pub cpu_millis: u32,
    pub memory_mib: u32,
    pub network: AgentNetwork,
}
impl Launch {
    pub fn validate(&self) -> Result<()> {
        if let Some(id) = self.image.strip_prefix("sha256:") {
            ensure!(
                id.len() == 64
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "invalid ACP image ID"
            );
        } else {
            crate::compute::ContainerSpec {
                image: self.image.clone(),
                command: self.command.clone(),
            }
            .validate()?;
        }
        ensure!(
            !self.command.is_empty()
                && self.command.len() <= 64
                && self
                    .command
                    .iter()
                    .all(|s| s.len() <= 4096 && !s.contains('\0'))
                && self.command.iter().map(String::len).sum::<usize>() <= 16384
                && self.command[0].starts_with('/'),
            "invalid pinned ACP image command"
        );
        ensure!(
            (100..=16000).contains(&self.cpu_millis) && (128..=16384).contains(&self.memory_mib),
            "invalid ACP process resources"
        );
        ensure!(
            [&self.agent_name, &self.agent_version, &self.binary_revision]
                .iter()
                .all(|s| !s.is_empty()
                    && s.len() <= 128
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"@/._+-".contains(&b))),
            "invalid ACP installed identity"
        );
        if self.adapter == Adapter::Codex {
            ensure!(
                self.binary_revision == crate::codex_bridge::CODEX_VERSION
                    && self.agent_name == "orbit-codex-acp"
                    && self.agent_version == "1",
                "unsupported Codex bridge installation"
            );
        } else if self.adapter == Adapter::Antigravity {
            ensure!(
                self.agent_name == "antigravity-acp",
                "unsupported Antigravity agent name"
            );
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<String> {
        Ok(digest(&serde_json::to_vec(self)?))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthStore {
    pub path: PathBuf,
    pub source: String,
    pub owner: String,
    pub account_class: String,
    /// Auth-store-relative source -> isolated agent HOME-relative destination.
    /// Config/plugin trees are not copied implicitly. All refresh writes are checked.
    pub files: BTreeMap<String, String>,
    #[serde(default)]
    pub scopes: Vec<crate::governance::Scope>,
}
pub fn relative_file(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1024
        && !path.contains('\0')
        && std::path::Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Runtime {
    pub binding_name: String,
    pub binding: Binding,
    pub launch: Launch,
    pub auth: AuthStore,
}
impl Runtime {
    pub fn validate(&self) -> Result<()> {
        self.binding.validate()?;
        self.launch.validate()?;
        let descriptor = self
            .binding
            .acp
            .as_ref()
            .context("ACP runtime requires an ACP binding")?;
        ensure!(
            valid_name(&self.binding_name) && descriptor.launch_digest == self.launch.digest()?,
            "ACP launch does not match pinned policy"
        );
        ensure!(
            self.auth.source == descriptor.auth.source
                && self.auth.owner == descriptor.auth.owner
                && self.auth.account_class == descriptor.auth.account_class,
            "ACP authentication identity mismatch"
        );
        ensure!(
            self.auth.path.is_absolute()
                && (1..=8).contains(&self.auth.files.len())
                && self.auth.files.iter().all(|(from, to)| relative_file(from)
                    && relative_file(to)
                    && !from.starts_with(".orbit-")
                    && !to.ends_with("config.toml")
                    && !to.ends_with("hooks.json")
                    && !to.starts_with("workspace/"))
                && self
                    .auth
                    .files
                    .values()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    == self.auth.files.len(),
            "invalid explicit ACP auth-file mapping"
        );
        ensure!(self.auth.scopes.len() <= 64, "too many ACP auth scopes");
        for scope in &self.auth.scopes {
            scope.validate()?;
        }
        if self.launch.adapter == Adapter::Codex {
            ensure!(
                self.binding.model.is_some()
                    && descriptor.agent_id == "codex"
                    && descriptor.agent_revision == crate::codex_bridge::REVISION
                    && self.auth.files.len() == 1
                    && self.auth.files.values().next().unwrap() == ".codex/auth.json",
                "Codex bridge requires exact model and isolated auth.json"
            );
        } else if self.launch.adapter == Adapter::Antigravity {
            ensure!(
                descriptor.agent_id == "antigravity-acp"
                    && (self.auth.files.is_empty()
                        || self
                            .auth
                            .files
                            .values()
                            .all(|dest| { dest.starts_with(".gemini/") })),
                "Antigravity adapter requires agent_id antigravity-acp and isolated .gemini auth files"
            );
        }
        Ok(())
    }

    /// Classifies an error that occurred during runtime execution into a normalized result.
    pub fn classify_error(
        &self,
        error: &anyhow::Error,
    ) -> crate::continuation::NormalizedAgentResult {
        let err_str = error.to_string();
        match self.launch.adapter {
            Adapter::Antigravity => crate::continuation::normalize_antigravity_error(&err_str),
            Adapter::Codex => crate::continuation::normalize_codex_error(&err_str),
            Adapter::Acp => {
                let lower = err_str.to_ascii_lowercase();
                if lower.contains("turn timeout") {
                    crate::continuation::NormalizedAgentResult::turn_limit(err_str)
                } else if lower.contains("429") {
                    crate::continuation::classify_http_429(&err_str, None)
                } else {
                    crate::continuation::NormalizedAgentResult::agent_error(err_str)
                }
            }
        }
    }

    pub fn authorize(&self, a: &Assignment) -> Result<()> {
        self.validate()?;
        let step = &a.plan.definition.steps[&a.step];
        let spec = step.agent.as_ref().context("ACP spec missing")?;
        ensure!(
            step.uses == "repository.code"
                && step.execution.is_some()
                && spec.binding == self.binding_name
                && a.agent_binding_digest.as_deref()
                    == Some(&digest(&serde_json::to_vec(&self.binding)?)),
            "ACP runtime does not match pinned assignment"
        );
        ensure!(
            matches!(spec.output, OutputContract::Object | OutputContract::Json),
            "ACP coding requires object output"
        );
        spec.authorize(&self.binding)?;
        let resources = step
            .resources
            .as_ref()
            .context("ACP step resources missing")?;
        ensure!(
            self.launch.cpu_millis <= resources.cpu_millis / 2
                && self.launch.memory_mib <= resources.memory_mib / 2,
            "ACP step reserves both agent and terminal resources; agent must fit within half"
        );
        ensure!(
            a.plan
                .scope
                .as_ref()
                .map_or(self.auth.scopes.is_empty(), |scope| self
                    .auth
                    .scopes
                    .contains(scope)),
            "ACP auth store denies execution scope"
        );
        Ok(())
    }

    pub async fn run(
        &self,
        session: crate::coding_agent::Session<'_>,
    ) -> Result<(crate::agent::AgentReport, Vec<u8>)> {
        self.authorize(session.assignment)?;
        tokio::task::LocalSet::new()
            .run_until(self.run_local(session))
            .await
    }

    async fn run_local(
        &self,
        session: crate::coding_agent::Session<'_>,
    ) -> Result<(crate::agent::AgentReport, Vec<u8>)> {
        use crate::acp_contract::RecordKind;
        use serde_json::json;
        use std::{os::unix::fs::OpenOptionsExt, process::Stdio, time::Duration};
        let a = session.assignment;
        let client = session.client;
        let spec = a.plan.definition.steps[&a.step].agent.as_ref().unwrap();
        let limits = spec.acp_limits.as_ref().unwrap();
        let request_path = session
            .directory
            .join(format!("acp-{}.json", crate::model::id()));
        let process_request = crate::acp_process::Request {
            runtime: self.clone(),
            attempt_id: a.attempt_id.clone(),
            timeout_seconds: a.plan.definition.steps[&a.step].timeout_seconds,
            tools: spec.tools.clone(),
        };
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&request_path)?;
            file.write_all(&serde_json::to_vec(&process_request)?)?;
            file.sync_all()?;
        }
        let mut command = tokio::process::Command::new(crate::worker::current_executable()?);
        command
            .arg("acp-supervisor")
            .arg("--request")
            .arg(&request_path)
            .current_dir(session.directory)
            .env_clear()
            .env(
                "PATH",
                std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
            )
            .env(
                "HOME",
                std::env::var_os("HOME").context("rootless runtime HOME missing")?,
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(false)
            .process_group(0);
        if let Some(value) = std::env::var_os("XDG_RUNTIME_DIR") {
            command.env("XDG_RUNTIME_DIR", value);
        }
        let mut child = command.spawn().with_context(|| {
            format!(
                "ACP supervisor launch failed: current_exe={:?}",
                std::env::current_exe()
            )
        })?;
        let mut wire = Wire::new(
            child.stdout.take().unwrap(),
            child.stdin.take().unwrap(),
            16 * 1024 * 1024,
        );
        let mut broker = Broker::new(session);
        let mut turn_count = 0;
        let result = async {
            let init = tokio::time::timeout(Duration::from_secs(30),acp_request(&mut wire,&mut broker,"initialize",json!({
                "protocolVersion":1,"clientInfo":{"name":"orbit","version":env!("CARGO_PKG_VERSION")},
                "clientCapabilities":{"fs":{"readTextFile":spec.tools.iter().any(|s|s=="read_file"),"writeTextFile":spec.tools.iter().any(|s|s=="write_file")},"terminal":spec.tools.iter().any(|s|s=="shell")}
            }))).await.context("ACP initialize timeout")??;
            ensure!(init["protocolVersion"] == 1 && init["agentInfo"]["name"] == self.launch.agent_name
                && init["agentInfo"]["version"] == self.launch.agent_version,"ACP installed identity mismatch");
            // Existing, explicitly provisioned auth only. Never invoke authenticate.
            let create_params=json!({"cwd":WORKSPACE,"mcpServers":[]});
            let created = tokio::time::timeout(Duration::from_secs(30),acp_request(&mut wire,&mut broker,"session/new",create_params)).await.context("ACP session creation timeout")??;
            let session_id = created["sessionId"].as_str().context("ACP session identity missing")?;
            ensure!(crate::agent::valid_name(session_id),"invalid ACP session identity");
            broker.session_id = Some(session_id.into());
            broker.session_digest = digest(format!("{}:{session_id}", a.attempt_id).as_bytes());
            broker.record(RecordKind::Started, &json!({
                "agent": self.launch.agent_name,
                "version": self.launch.agent_version,
                "launch_digest": self.launch.digest()?,
                "model": self.binding.model
            }), 0, 0).await?;
            let current_model = created
                .get("models")
                .and_then(|m| m.get("currentModelId"))
                .and_then(|v| v.as_str());
            let model_confirmed = select_model(
                &mut wire,
                &mut broker,
                session_id,
                self.binding.model.as_deref(),
                current_model,
            )
            .await?;
            if model_confirmed
                && let (Some(execution_id), Some(actual_model)) =
                    (a.execution_id.as_deref(), self.binding.model.as_deref())
            {
                client
                    .operation(
                        a,
                        crate::model::Action::UpdateExecution {
                            execution_id: execution_id.to_string(),
                            actual_model: Some(actual_model.to_string()),
                            turn_count: None,
                            tool_call_count: None,
                            tool_success_count: None,
                            tool_failure_count: None,
                            tool_counts: std::collections::BTreeMap::new(),
                        },
                    )
                    .await?;
            }
            if self.launch.adapter == Adapter::Antigravity {
                let _ = tokio::time::timeout(
                    Duration::from_secs(10),
                    acp_request(&mut wire, &mut broker, "session/set_mode", json!({
                        "sessionId": session_id,
                        "modeId": "yolo"
                    }))
                ).await.context("Antigravity set_mode timeout")??;
            }
            let prompt = json!({"sessionId":session_id,"prompt":[{"type":"text","text":format!(
                "Task: {}\nPinned base: {}\nContext: {}\nUse only client file and terminal callbacks. Preserve tests. Do not push, deploy, delegate, or install anything. Finish with a concise summary for independent verification.",
                a.plan.definition.inputs.task,a.plan.definition.inputs.base_revision,spec.context)}]});
            ensure!(serde_json::to_vec(&prompt)?.len() <= 262144,"ACP prompt too large");
            let call = broker.reserve(None,&prompt,0).await?;
            turn_count = 1;
            broker.active=true;
            let response = tokio::time::timeout(Duration::from_secs(limits.turn_timeout_seconds),acp_request(&mut wire,&mut broker,"session/prompt",prompt)).await;
            let response = match response {
                Ok(Ok(val)) => val,
                Ok(Err(err)) => {
                    if err.downcast_ref::<crate::acp_broker::ExecutionLimitError>().is_some()
                        || broker.execution_limit.is_some()
                    {
                        json!({"stopReason": "budget_exhausted"})
                    } else {
                        return Err(err);
                    }
                }
                Err(_timeout) => {
                    let diagnostic = broker.timeout_diagnostic();
                    let _ = tokio::time::timeout(
                        Duration::from_secs(1),
                        wire.notify("session/cancel", json!({"sessionId": session_id})),
                    )
                    .await;
                    anyhow::bail!(
                        "ACP turn timeout; prompt outcome unconfirmed; {diagnostic}"
                    );
                }
            };
            let stop_reason = response["stopReason"].as_str().unwrap_or("unknown");
            ensure!(
                ["end_turn", "budget_exhausted"].contains(&stop_reason),
                "ACP turn did not complete: {stop_reason}"
            );
            broker.close_terminals().await?;
            broker.active = false;
            ensure!(!broker.poisoned, "ACP broker failed");
            Ok::<_, anyhow::Error>((call, response))
        }.await;
        // Dropping the only protocol writer closes the independent supervisor's
        // lifeline. A future dropped by lease loss follows the same cleanup path.
        drop(wire);
        let cleanup = tokio::time::timeout(Duration::from_secs(60), child.wait())
            .await
            .ok()
            .and_then(Result::ok)
            .and_then(|s| s.code());
        ensure!(
            cleanup.is_some_and(|code| crate::acp_process::read_cleanup(
                &request_path,
                Some(&a.attempt_id)
            )
            .ok()
                == Some(code)),
            "ACP process/auth cleanup unconfirmed; auth store may be quarantined"
        );
        let launch_diagnostic =
            crate::acp_process::read_cleanup_diagnostic(&request_path, Some(&a.attempt_id))
                .ok()
                .flatten();
        if let Some(execution_id) = a.execution_id.as_deref() {
            let _ = client
                .operation(
                    a,
                    crate::model::Action::UpdateExecution {
                        execution_id: execution_id.to_string(),
                        actual_model: None,
                        turn_count: Some(turn_count),
                        tool_call_count: Some(broker.tool_calls),
                        tool_success_count: Some(broker.tool_successes),
                        tool_failure_count: Some(broker.tool_failures),
                        tool_counts: broker.tool_counts.clone(),
                    },
                )
                .await;
        }
        let result = result.map_err(|error| {
            if let Some(diagnostic) = launch_diagnostic {
                anyhow::anyhow!("{error:#}; launch diagnostics: {diagnostic}")
            } else {
                error
            }
        });
        let (call, response) = result?;
        broker.finish(&call, &response).await?;
        broker
            .record(RecordKind::Completed, &response, 0, 0)
            .await?;
        let stop_reason = response["stopReason"].as_str().unwrap_or("end_turn");
        let report = crate::agent::AgentReport {
            attempt_id: a.attempt_id.clone(),
            binding_digest: a.agent_binding_digest.clone().unwrap(),
            output: json!({"summary":"ACP turn completed; inspect the patch and independent verification.","acp":{
                "session_digest":broker.session_digest,"agent":self.launch.agent_name,"version":self.launch.agent_version,
                "launch_digest":self.launch.digest()?,"model":self.binding.model,
                "model_attribution":if self.binding.model.is_some(){"agent_confirmed_exact"}else{"agent_configured_unverified"},
                "accounting":"execution_only","tokens":null,"cost_microusd":null,"stop_reason":stop_reason,
                "cleanup_confirmed":true,"output_bytes":broker.output_bytes,"reported_tool_calls":broker.reported_tools,
                "tool_calls":broker.tool_calls,"tool_successes":broker.tool_successes,"tool_failures":broker.tool_failures,"tool_counts":broker.tool_counts
            }}),
            delegation_inputs: vec![],
        };
        Ok((report, broker.log))
    }
}

pub async fn acp_request(
    wire: &mut Wire,
    broker: &mut Broker<'_>,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    let id = wire.request(method, params).await?;
    loop {
        let value = wire.read().await?;
        if value.get("method").is_some() {
            broker.message(wire, value).await?;
        } else {
            return Wire::result(value, &id);
        }
    }
}

pub async fn select_model(
    wire: &mut Wire,
    broker: &mut Broker<'_>,
    session_id: &str,
    requested_model: Option<&str>,
    current_model: Option<&str>,
) -> Result<bool> {
    let Some(model) = requested_model else {
        return Ok(false);
    };
    ensure!(
        crate::agent::valid_name(model),
        "invalid requested model name"
    );
    if current_model != Some(model) {
        let mut confirmed = false;
        let mut evidence_confirmed = false;

        // 1. Try session/set_config_option
        let set_config_res = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            acp_request(
                wire,
                broker,
                "session/set_config_option",
                serde_json::json!({
                    "sessionId": session_id,
                    "configId": "model",
                    "value": model,
                }),
            ),
        )
        .await;

        ensure!(
            !broker.poisoned,
            "ACP broker poisoned during model selection"
        );

        if let Ok(Ok(config_resp)) = set_config_res
            && let Some(options) = config_resp.get("configOptions").and_then(|v| v.as_array())
        {
            for opt in options {
                if opt.get("id").and_then(|v| v.as_str()) == Some("model")
                    && opt.get("currentValue").and_then(|v| v.as_str()) == Some(model)
                {
                    confirmed = true;
                    evidence_confirmed = true;
                    break;
                }
            }
        }

        // 2. Fall back to session/set_model
        if !confirmed {
            let set_model_res = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                acp_request(
                    wire,
                    broker,
                    "session/set_model",
                    serde_json::json!({
                        "sessionId": session_id,
                        "modelId": model,
                    }),
                ),
            )
            .await
            .context("ACP set_model timeout")??;

            ensure!(
                !broker.poisoned,
                "ACP broker poisoned during model selection"
            );

            if let Some(curr) = set_model_res
                .get("models")
                .and_then(|m| m.get("currentModelId"))
                .and_then(|v| v.as_str())
            {
                ensure!(
                    curr == model,
                    "ACP set_model response returned mismatched model: expected {}, got {}",
                    model,
                    curr
                );
                evidence_confirmed = true;
            }
            confirmed = true;
        }

        ensure!(
            confirmed,
            "ACP requested model could not be activated or confirmed: {}",
            model
        );
        return Ok(evidence_confirmed);
    }

    Ok(true)
}
