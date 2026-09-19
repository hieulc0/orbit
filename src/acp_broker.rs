//! The only repository-effect authority exposed to an ACP peer.
use crate::{
    acp_contract::{Charge, Record, RecordBatch, RecordKind},
    acp_terminal::Terminal,
    agent::{CallReceipt, CallReservation},
    coding_agent::Session,
    model::*,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

struct Handle {
    terminal: Rc<Terminal>,
    call: String,
    finished: bool,
}
pub struct Broker<'a> {
    pub session: Session<'a>,
    pub session_id: Option<String>,
    pub session_digest: String,
    pub active: bool,
    pub poisoned: bool,
    pub log: Vec<u8>,
    pub output_bytes: u64,
    pub reported_tools: u32,
    sequence: u32,
    terminals: BTreeMap<String, Handle>,
    seen_requests: BTreeSet<String>,
    seen_tools: BTreeSet<String>,
}
impl<'a> Broker<'a> {
    pub fn new(session: Session<'a>) -> Self {
        Self {
            session,
            session_id: None,
            session_digest: String::new(),
            active: false,
            poisoned: false,
            log: vec![],
            output_bytes: 0,
            reported_tools: 0,
            sequence: 0,
            terminals: BTreeMap::new(),
            seen_requests: BTreeSet::new(),
            seen_tools: BTreeSet::new(),
        }
    }
    fn limits(&self) -> &crate::acp_contract::Limits {
        self.session.assignment.plan.definition.steps[&self.session.assignment.step]
            .agent
            .as_ref()
            .unwrap()
            .acp_limits
            .as_ref()
            .unwrap()
    }
    pub async fn record(
        &mut self,
        kind: RecordKind,
        value: &Value,
        bytes: u64,
        tools: u32,
    ) -> Result<()> {
        let record = Record {
            kind,
            digest: digest(&serde_json::to_vec(value)?),
            output_bytes: bytes,
            reported_tool_calls: tools,
        };
        let batch = RecordBatch {
            attempt_id: self.session.assignment.attempt_id.clone(),
            session_digest: self.session_digest.clone(),
            sequence: self.sequence,
            records: vec![record],
        };
        self.session
            .client
            .operation(
                self.session.assignment,
                Action::RecordAcpSession {
                    batch: batch.clone(),
                },
            )
            .await?;
        self.sequence += 1;
        self.output_bytes += bytes;
        self.reported_tools += tools;
        let bytes = serde_json::to_vec(&batch)?;
        ensure!(
            self.log.len() + bytes.len() < 2 * 1024 * 1024,
            "ACP transcript bound exceeded"
        );
        self.log.extend(bytes);
        self.log.push(b'\n');
        Ok(())
    }
    pub async fn reserve(&self, tool: Option<&str>, value: &Value, seconds: u64) -> Result<String> {
        let call = format!("{}-{}", self.session.assignment.attempt_id, id());
        let response = self
            .session
            .client
            .operation(
                self.session.assignment,
                Action::ReserveAgentCall {
                    reservation: CallReservation {
                        call_id: call.clone(),
                        tokens: None,
                        cost_microusd: None,
                        tool: tool.map(str::to_owned),
                        permissions: tool
                            .map(|t| {
                                crate::coding_agent::tool_permissions(t)
                                    .unwrap()
                                    .iter()
                                    .map(|s| s.to_string())
                                    .collect()
                            })
                            .unwrap_or_default(),
                        request_digest: Some(digest(&serde_json::to_vec(value)?)),
                        acp_charge: Some(if tool.is_some() {
                            Charge::Broker {
                                terminal_runtime_seconds: seconds,
                            }
                        } else {
                            Charge::Prompt
                        }),
                    },
                },
            )
            .await?;
        ensure!(
            response["replayed"] == false,
            "replayed call cannot dispatch"
        );
        Ok(call)
    }
    pub async fn finish(&self, call: &str, value: &Value) -> Result<()> {
        self.session
            .client
            .operation(
                self.session.assignment,
                Action::FinishAgentCall {
                    receipt: CallReceipt {
                        call_id: call.into(),
                        attempt_id: self.session.assignment.attempt_id.clone(),
                        result_digest: digest(&serde_json::to_vec(value)?),
                        external_id: None,
                    },
                },
            )
            .await?;
        Ok(())
    }
    fn path(&self, value: &Value) -> Result<String> {
        let path = std::path::Path::new(value.as_str().context("ACP path missing")?);
        let relative = path
            .strip_prefix(crate::acp_runtime::WORKSPACE)
            .context("ACP path outside workspace")?
            .to_str()
            .context("non UTF-8 path")?;
        let relative = relative.trim_end_matches('/');
        let relative = if relative.is_empty() { "." } else { relative };
        ensure!(
            crate::acp_runtime::relative_file(relative),
            "invalid ACP relative path"
        );
        Ok(relative.into())
    }
    pub async fn message(&mut self, wire: &mut crate::acp_wire::Wire, value: Value) -> Result<()> {
        let method = value["method"].as_str().context("ACP method missing")?;
        if let Some(id) = value.get("id") {
            let key = serde_json::to_string(id)?;
            ensure!(
                (id.is_string() || id.is_i64())
                    && key.len() <= 256
                    && self.seen_requests.len() < 8192
                    && self.seen_requests.insert(key),
                "duplicate or invalid ACP request ID"
            );
            let result = self.callback(method, &value["params"]).await;
            self.poisoned |= result.is_err();
            wire.response(id.clone(), result).await?;
            ensure!(
                !self.poisoned,
                "ACP broker denied or could not confirm a callback"
            );
        } else {
            ensure!(method == "session/update", "unsupported ACP notification");
            let params = &value["params"];
            self.owner(params)?;
            let _: agent_client_protocol::SessionNotification =
                serde_json::from_value(params.clone())
                    .map_err(|_| anyhow::anyhow!("invalid ACP session update"))?;
            let update = &params["update"];
            let kind = update["sessionUpdate"]
                .as_str()
                .context("ACP update kind missing")?;
            let mut tools = 0;
            if kind == "tool_call" {
                let id = update["toolCallId"]
                    .as_str()
                    .context("reported tool identity missing")?;
                ensure!(
                    id.len() <= 256 && self.seen_tools.insert(digest(id.as_bytes())),
                    "duplicate reported tool"
                );
                tools = 1;
            }
            // Persist counts and digests only, never raw thoughts/auth/provider data.
            self.record(
                RecordKind::Update,
                update,
                serde_json::to_vec(update)?.len() as u64,
                tools,
            )
            .await?;
        }
        Ok(())
    }
    fn owner(&self, params: &Value) -> Result<()> {
        ensure!(
            self.session_id.as_deref() == params["sessionId"].as_str(),
            "foreign or inactive ACP session"
        );
        Ok(())
    }
    async fn callback(&mut self, method: &str, params: &Value) -> Result<Value> {
        self.owner(params)?;
        ensure!(self.active, "callbacks require active turn");
        match method {
            "fs/read_text_file" | "fs/write_text_file" => {
                ensure!(
                    self.terminals.is_empty(),
                    "file callbacks require released terminals"
                );
                let path = self.path(&params["path"])?;
                let root = crate::acp_files::Root::open(&self.session.workspace.path)?;
                let tool = if method == "fs/read_text_file" {
                    "read_file"
                } else {
                    "write_file"
                };
                if tool == "write_file" {
                    ensure!(
                        params["content"].as_str().is_some_and(|s| s.len() <= 65536),
                        "ACP file content bound exceeded"
                    );
                }
                let call = self.reserve(Some(tool), params, 0).await?;
                let result = if tool == "read_file" {
                    let text = String::from_utf8(root.read(&path, 65536)?)
                        .context("ACP file is not UTF-8")?;
                    let line = params
                        .get("line")
                        .filter(|value| !value.is_null())
                        .map(|v| v.as_u64().filter(|n| *n > 0).context("invalid ACP line"))
                        .transpose()?
                        .unwrap_or(1);
                    let limit = params
                        .get("limit")
                        .filter(|value| !value.is_null())
                        .map(|v| {
                            v.as_u64()
                                .filter(|n| *n > 0)
                                .context("invalid ACP line limit")
                        })
                        .transpose()?
                        .unwrap_or(65536);
                    ensure!(line <= 65536 && limit <= 65536, "ACP line bounds exceeded");
                    json!({"content":text.split_inclusive('\n').skip((line-1) as usize).take(limit as usize).collect::<String>()})
                } else {
                    root.write(&path, params["content"].as_str().unwrap().as_bytes())?;
                    json!({})
                };
                self.record(
                    RecordKind::BrokerOutput,
                    &result,
                    serde_json::to_vec(&result)?.len() as u64,
                    0,
                )
                .await?;
                self.finish(&call, &result).await?;
                Ok(result)
            }
            "terminal/create" => {
                let request: agent_client_protocol::CreateTerminalRequest =
                    serde_json::from_value(params.clone())
                        .map_err(|_| anyhow::anyhow!("invalid ACP terminal request"))?;
                ensure!(
                    self.terminals.is_empty() && request.env.is_empty(),
                    "one terminal with no agent environment allowed"
                );
                let cwd = match request.cwd {
                    Some(ref p) if p == std::path::Path::new(crate::acp_runtime::WORKSPACE) => {
                        ".".into()
                    }
                    Some(p) => self.path(&json!(p))?,
                    None => ".".into(),
                };
                let command = CommandSpec {
                    argv: std::iter::once(request.command)
                        .chain(request.args)
                        .collect(),
                    cwd,
                    timeout_seconds: self.limits().terminal_timeout_seconds,
                };
                crate::execution::validate_tool_command(&command)?;
                let output_limit = request.output_byte_limit.unwrap_or(65536).min(65536) as usize;
                ensure!(output_limit > 0, "empty ACP output allowance");
                let call = self
                    .reserve(Some("shell"), params, command.timeout_seconds)
                    .await?;
                let spec = crate::workspace::prepare_execution(
                    self.session.assignment,
                    &self.session.workspace.path,
                    self.session.directory,
                    self.session.profile,
                    &command,
                )
                .await?;
                let terminal = Terminal::start(
                    &spec,
                    self.session.directory,
                    output_limit,
                    self.limits().output_bytes.saturating_sub(self.output_bytes),
                )
                .await?;
                let id = id();
                self.terminals.insert(
                    id.clone(),
                    Handle {
                        terminal: Rc::new(terminal),
                        call,
                        finished: false,
                    },
                );
                Ok(json!({"terminalId":id}))
            }
            "terminal/output" | "terminal/wait_for_exit" | "terminal/kill" | "terminal/release" => {
                let id = params["terminalId"]
                    .as_str()
                    .context("terminal ID missing")?;
                let terminal = self
                    .terminals
                    .get(id)
                    .context("foreign or released terminal")?
                    .terminal
                    .clone();
                let output = match method {
                    "terminal/output" => terminal.output(),
                    "terminal/wait_for_exit" => terminal.wait().await?,
                    _ => terminal.kill().await?,
                };
                let exit = output.exit_code.map(|code| json!({"exitCode":code}));
                let result = match method {
                    "terminal/output" => {
                        let mut value =
                            json!({"output":output.text(),"truncated":output.truncated});
                        if let Some(exit) = &exit {
                            value["exitStatus"] = exit.clone();
                        }
                        value
                    }
                    "terminal/wait_for_exit" => {
                        json!({"exitStatus":exit.context("terminal exit unavailable")?})
                    }
                    _ => json!({}),
                };
                if output.complete && !self.terminals[id].finished {
                    ensure!(
                        output.cleanup_confirmed && !output.overflow,
                        "terminal cleanup or output unconfirmed"
                    );
                    let receipt = json!({"exit_code":output.exit_code,"output_digest":digest(&output.bytes),"total_bytes":output.total,"cleanup_confirmed":true});
                    self.record(RecordKind::BrokerOutput, &receipt, output.total, 0)
                        .await?;
                    self.finish(&self.terminals[id].call, &receipt).await?;
                    self.terminals.get_mut(id).unwrap().finished = true;
                }
                if method == "terminal/release" {
                    self.terminals.remove(id);
                }
                Ok(result)
            }
            // Native approval is not a broker dispatch. No implicit auth, MCP,
            // extensions, URL opening or delegated process execution is permitted.
            _ => anyhow::bail!("unsupported ACP client operation"),
        }
    }
    pub async fn close_terminals(&mut self) -> Result<()> {
        for id in self.terminals.keys().cloned().collect::<Vec<_>>() {
            self.callback(
                "terminal/release",
                &json!({"sessionId":self.session_id,"terminalId":id}),
            )
            .await?;
        }
        Ok(())
    }
}
