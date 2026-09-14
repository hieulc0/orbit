//! Operator-owned ACP installation registry. No executable or auth path enters a plan.
use crate::{
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
            .all(|c| matches!(c, Component::Normal(_)))
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
        }
        Ok(())
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
        use crate::{acp_broker::Broker, acp_contract::RecordKind, acp_wire::Wire};
        use serde_json::{Value, json};
        use std::{os::unix::fs::OpenOptionsExt, process::Stdio, time::Duration};
        let a = session.assignment;
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
        let mut command = tokio::process::Command::new(std::env::current_exe()?);
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
        let mut child = command.spawn().context("ACP supervisor launch failed")?;
        let mut wire = Wire::new(
            child.stdout.take().unwrap(),
            child.stdin.take().unwrap(),
            16 * 1024 * 1024,
        );
        let mut broker = Broker::new(session);
        async fn request(
            wire: &mut Wire,
            broker: &mut Broker<'_>,
            method: &str,
            params: Value,
        ) -> Result<Value> {
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
        let result = async {
            let init = tokio::time::timeout(Duration::from_secs(30),request(&mut wire,&mut broker,"initialize",json!({
                "protocolVersion":1,"clientInfo":{"name":"orbit","version":env!("CARGO_PKG_VERSION")},
                "clientCapabilities":{"fs":{"readTextFile":spec.tools.iter().any(|s|s=="read_file"),"writeTextFile":spec.tools.iter().any(|s|s=="write_file")},"terminal":spec.tools.iter().any(|s|s=="shell")}
            }))).await.context("ACP initialize timeout")??;
            ensure!(init["protocolVersion"] == 1 && init["agentInfo"]["name"] == self.launch.agent_name
                && init["agentInfo"]["version"] == self.launch.agent_version,"ACP installed identity mismatch");
            // Existing, explicitly provisioned auth only. Never invoke authenticate.
            let create_params=json!({"cwd":WORKSPACE,"mcpServers":[]});
            let created = tokio::time::timeout(Duration::from_secs(30),request(&mut wire,&mut broker,"session/new",create_params)).await.context("ACP session creation timeout")??;
            let session_id = created["sessionId"].as_str().context("ACP session identity missing")?;
            ensure!(crate::agent::valid_name(session_id),"invalid ACP session identity");
            if let Some(model) = &self.binding.model {
                ensure!(created["models"]["currentModelId"] == *model,"ACP exact model not confirmed by session");
            }
            broker.session_id=Some(session_id.into());
            broker.session_digest=digest(format!("{}:{session_id}",a.attempt_id).as_bytes());
            broker.record(RecordKind::Started,&json!({"agent":self.launch.agent_name,"version":self.launch.agent_version,
                "launch_digest":self.launch.digest()?,"model":self.binding.model}),0,0).await?;
            let prompt = json!({"sessionId":session_id,"prompt":[{"type":"text","text":format!(
                "Task: {}\nPinned base: {}\nContext: {}\nUse only client file and terminal callbacks. Preserve tests. Do not push, deploy, delegate, or install anything. Finish with a concise summary for independent verification.",
                a.plan.definition.inputs.task,a.plan.definition.inputs.base_revision,spec.context)}]});
            ensure!(serde_json::to_vec(&prompt)?.len() <= 262144,"ACP prompt too large");
            let call = broker.reserve(None,&prompt,0).await?;
            broker.active=true;
            let response = tokio::time::timeout(Duration::from_secs(limits.turn_timeout_seconds),request(&mut wire,&mut broker,"session/prompt",prompt)).await;
            if response.is_err() {
                let _ = tokio::time::timeout(Duration::from_secs(1),wire.notify("session/cancel",json!({"sessionId":session_id}))).await;
            }
            let response = response.context("ACP turn timeout; prompt outcome unconfirmed")??;
            ensure!(response["stopReason"] == "end_turn","ACP turn did not complete");
            broker.close_terminals().await?;
            broker.active=false;
            ensure!(!broker.poisoned,"ACP broker failed");
            Ok::<_,anyhow::Error>((call,response))
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
        let (call, response) = result?;
        broker.finish(&call, &response).await?;
        broker
            .record(RecordKind::Completed, &response, 0, 0)
            .await?;
        let report = crate::agent::AgentReport {
            attempt_id: a.attempt_id.clone(),
            binding_digest: a.agent_binding_digest.clone().unwrap(),
            output: json!({"summary":"ACP turn completed; inspect the patch and independent verification.","acp":{
                "session_digest":broker.session_digest,"agent":self.launch.agent_name,"version":self.launch.agent_version,
                "launch_digest":self.launch.digest()?,"model":self.binding.model,
                "model_attribution":if self.binding.model.is_some(){"agent_confirmed_exact"}else{"agent_configured_unverified"},
                "accounting":"execution_only","tokens":null,"cost_microusd":null,"stop_reason":"end_turn",
                "cleanup_confirmed":true,"output_bytes":broker.output_bytes,"reported_tool_calls":broker.reported_tools
            }}),
            delegation_inputs: vec![],
        };
        Ok((report, broker.log))
    }
}
