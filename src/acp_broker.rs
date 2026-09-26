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
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug)]
pub enum BrokerError {
    Recoverable {
        code: i64,
        message: String,
    },
    ExecutionLimit {
        reason: crate::continuation::TerminationReason,
        message: String,
    },
    Fatal(anyhow::Error),
}

impl BrokerError {
    pub fn recoverable(code: i64, message: impl Into<String>) -> Self {
        Self::Recoverable {
            code,
            message: message.into(),
        }
    }
    pub fn execution_limit(
        reason: crate::continuation::TerminationReason,
        message: impl Into<String>,
    ) -> Self {
        Self::ExecutionLimit {
            reason,
            message: message.into(),
        }
    }
    pub fn fatal(err: impl Into<anyhow::Error>) -> Self {
        Self::Fatal(err.into())
    }
    pub fn from_reserve_error(err: anyhow::Error) -> Self {
        if let Some(crate::worker::ClientError::BudgetExhausted { message }) =
            err.downcast_ref::<crate::worker::ClientError>()
        {
            Self::ExecutionLimit {
                reason: crate::continuation::TerminationReason::BudgetExhausted,
                message: message.clone(),
            }
        } else {
            Self::Fatal(err)
        }
    }
}

impl std::fmt::Display for BrokerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Recoverable { code, message } => {
                write!(f, "recoverable callback error ({code}): {message}")
            }
            Self::ExecutionLimit { reason, message } => {
                write!(f, "execution limit reached ({reason:?}): {message}")
            }
            Self::Fatal(err) => write!(f, "fatal broker error: {err:#}"),
        }
    }
}

impl std::error::Error for BrokerError {}

#[derive(Debug, Clone)]
pub struct ExecutionLimitError {
    pub reason: crate::continuation::TerminationReason,
    pub message: String,
}

impl std::fmt::Display for ExecutionLimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "execution limit reached ({:?}): {}",
            self.reason, self.message
        )
    }
}

impl std::error::Error for ExecutionLimitError {}

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
    pub agent_output: String,
    /// Resolved callbacks (including rejections and terminal suboperations),
    /// not accepted effect reservations. Successes + failures == calls.
    pub tool_calls: u64,
    pub tool_successes: u64,
    pub tool_failures: u64,
    pub tool_counts: BTreeMap<String, u64>,
    pub execution_limit: Option<(crate::continuation::TerminationReason, String)>,
    /// Bounded protocol state retained for diagnosing an uncertain ACP turn.
    /// These fields contain no prompt, tool argument, or credential material.
    pub last_activity_epoch_ms: Option<u64>,
    last_activity_kind: Option<&'static str>,
    pub turn_started_epoch_ms: Option<u64>,
    pub pending_model_call: bool,
    pub pending_tool_callbacks: u32,
    pending_model_call_id: Option<String>,
    pending_tool_call_ids: BTreeSet<String>,
    callback_in_flight: bool,
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
            agent_output: String::new(),
            tool_calls: 0,
            tool_successes: 0,
            tool_failures: 0,
            tool_counts: BTreeMap::new(),
            execution_limit: None,
            last_activity_epoch_ms: None,
            last_activity_kind: None,
            turn_started_epoch_ms: None,
            pending_model_call: false,
            pending_tool_callbacks: 0,
            pending_model_call_id: None,
            pending_tool_call_ids: BTreeSet::new(),
            callback_in_flight: false,
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
    fn touch(&mut self, kind: &'static str) {
        self.last_activity_epoch_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| u64::try_from(duration.as_millis()).ok());
        self.last_activity_kind = Some(kind);
    }
    pub fn timeout_diagnostic(&self) -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| u64::try_from(duration.as_millis()).ok());
        let elapsed = self
            .turn_started_epoch_ms
            .zip(now)
            .map(|(started, now)| now.saturating_sub(started));
        format!(
            "session_digest={} turn_elapsed_ms={:?} last_activity_epoch_ms={:?} last_activity_kind={} pending_model_call={} pending_tool_reservations={} active_tool_callbacks={} broker_poisoned={} active_terminals={} runtime_process_state=unknown",
            self.session_id
                .as_ref()
                .map(|session_id| digest(
                    format!("{}:{session_id}", self.session.assignment.attempt_id).as_bytes()
                ))
                .unwrap_or_else(|| "unknown".into()),
            elapsed,
            self.last_activity_epoch_ms,
            self.last_activity_kind.unwrap_or("none"),
            self.pending_model_call,
            self.pending_tool_callbacks,
            u8::from(self.callback_in_flight),
            self.poisoned,
            self.terminals.len(),
        )
    }
    pub async fn record(
        &mut self,
        kind: RecordKind,
        value: &Value,
        bytes: u64,
        tools: u32,
    ) -> Result<()> {
        self.touch(match &kind {
            RecordKind::Started => "record_started",
            RecordKind::Update => "record_update",
            RecordKind::BrokerOutput => "record_broker_output",
            RecordKind::Completed => "record_completed",
        });
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
    pub async fn reserve(
        &mut self,
        tool: Option<&str>,
        value: &Value,
        seconds: u64,
    ) -> Result<String> {
        let call = format!("{}-{}", self.session.assignment.attempt_id, id());
        let is_model = tool.is_none();
        if is_model {
            self.pending_model_call = true;
            self.pending_model_call_id = Some(call.clone());
            self.turn_started_epoch_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .and_then(|duration| u64::try_from(duration.as_millis()).ok());
            self.touch("model_call_reservation");
        } else {
            self.pending_tool_callbacks = self.pending_tool_callbacks.saturating_add(1);
            self.pending_tool_call_ids.insert(call.clone());
            self.touch("tool_call_reservation");
        }
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
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                // A lost acknowledgement can hide an accepted reservation.
                // Only a definite budget rejection proves no charge was made.
                if matches!(
                    error.downcast_ref::<crate::worker::ClientError>(),
                    Some(crate::worker::ClientError::BudgetExhausted { .. })
                ) {
                    self.clear_pending_call(&call);
                }
                return Err(error);
            }
        };
        if response["replayed"] != false {
            anyhow::bail!("replayed call cannot dispatch");
        }
        Ok(call)
    }
    fn clear_pending_call(&mut self, call: &str) {
        if self.pending_model_call_id.as_deref() == Some(call) {
            self.pending_model_call = false;
            self.pending_model_call_id = None;
        } else if self.pending_tool_call_ids.remove(call) {
            self.pending_tool_callbacks -= 1;
        }
    }
    pub async fn finish(&mut self, call: &str, value: &Value) -> Result<()> {
        self.touch("call_finished");
        let result = self
            .session
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
            .await;
        if result.is_ok() {
            self.clear_pending_call(call);
        }
        result.map(|_| ())
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
        self.touch(if value.get("id").is_some() {
            "agent_callback"
        } else {
            "session_notification"
        });
        if let Some(id) = value.get("id") {
            let key = serde_json::to_string(id)?;
            ensure!(
                (id.is_string() || id.is_i64())
                    && key.len() <= 256
                    && self.seen_requests.len() < 8192
                    && self.seen_requests.insert(key),
                "duplicate or invalid ACP request ID"
            );
            self.callback_in_flight = true;
            let result = self.callback(method, &value["params"]).await;
            self.callback_in_flight = false;
            // Count the same population for totals, outcomes and per-tool counts.
            // A dropped callback remains in flight, with no invented outcome.
            self.tool_calls += 1;
            let tool = match method {
                "fs/read_text_file" => "read_file",
                "fs/write_text_file" => "write_file",
                "terminal/create" => "shell",
                "terminal/output" => "terminal/output",
                "terminal/wait_for_exit" => "terminal/wait_for_exit",
                "terminal/kill" => "terminal/kill",
                "terminal/release" => "terminal/release",
                _ => "unsupported",
            };
            *self.tool_counts.entry(tool.into()).or_default() += 1;
            match result {
                Ok(val) => {
                    self.tool_successes += 1;
                    wire.response_ok(id.clone(), val).await?;
                }
                Err(BrokerError::Recoverable { code, message }) => {
                    self.tool_failures += 1;
                    let err_val = json!({
                        "error": {
                            "code": code,
                            "message": &message,
                        }
                    });
                    let _ = self
                        .record(
                            RecordKind::BrokerOutput,
                            &err_val,
                            serde_json::to_vec(&err_val)
                                .map(|v| v.len() as u64)
                                .unwrap_or(0),
                            0,
                        )
                        .await;
                    wire.response_error(id.clone(), code, &message).await?;
                }
                Err(BrokerError::ExecutionLimit { reason, message }) => {
                    self.tool_failures += 1;
                    self.execution_limit = Some((reason, message.clone()));
                    let err_val = json!({
                        "error": {
                            "code": -32000,
                            "message": &message,
                            "execution_limit": true,
                        }
                    });
                    let _ = self
                        .record(
                            RecordKind::BrokerOutput,
                            &err_val,
                            serde_json::to_vec(&err_val)
                                .map(|v| v.len() as u64)
                                .unwrap_or(0),
                            0,
                        )
                        .await;
                    let _ = wire.response_error(id.clone(), -32000, &message).await;
                    if let Some(session_id) = &self.session_id {
                        let _ = wire
                            .notify("session/cancel", json!({ "sessionId": session_id }))
                            .await;
                    }
                    return Err(ExecutionLimitError { reason, message }.into());
                }
                Err(BrokerError::Fatal(err)) => {
                    self.tool_failures += 1;
                    self.poisoned = true;
                    let detail = format!("{err:#}");
                    let _ = wire.response_error(id.clone(), -32603, &detail).await;
                    anyhow::bail!("ACP broker fatal error: {}", detail);
                }
            }
        } else {
            if method != "session/update" {
                self.poisoned = true;
                anyhow::bail!("unsupported ACP notification");
            }
            let params = &value["params"];
            if let Err(e) = self.owner(params) {
                self.poisoned = true;
                anyhow::bail!("ACP broker fatal error: {e}");
            }
            let _: agent_client_protocol::SessionNotification =
                match serde_json::from_value(params.clone()) {
                    Ok(n) => n,
                    Err(_) => {
                        self.poisoned = true;
                        anyhow::bail!("invalid ACP session update");
                    }
                };
            let update = &params["update"];
            let kind = match update["sessionUpdate"].as_str() {
                Some(k) => k,
                None => {
                    self.poisoned = true;
                    anyhow::bail!("ACP update kind missing");
                }
            };
            let mut tools = 0;
            if kind == "tool_call" {
                let id = match update["toolCallId"].as_str() {
                    Some(id) => id,
                    None => {
                        self.poisoned = true;
                        anyhow::bail!("reported tool identity missing");
                    }
                };
                if id.len() > 256 || !self.seen_tools.insert(digest(id.as_bytes())) {
                    self.poisoned = true;
                    anyhow::bail!("duplicate reported tool");
                }
                tools = 1;
            }
            if kind == "agent_message_chunk" {
                let chunk_text = update
                    .get("content")
                    .and_then(|c| c.get("text"))
                    .and_then(|t| t.as_str())
                    .or_else(|| update.get("text").and_then(|t| t.as_str()));
                if let Some(text) =
                    chunk_text.filter(|t| self.agent_output.len() + t.len() <= 512 * 1024)
                {
                    self.agent_output.push_str(text);
                }
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
    fn owner(&self, params: &Value) -> Result<(), BrokerError> {
        if self.session_id.as_deref() != params.get("sessionId").and_then(|v| v.as_str()) {
            return Err(BrokerError::fatal(anyhow::anyhow!(
                "foreign or inactive ACP session"
            )));
        }
        Ok(())
    }
    async fn callback(&mut self, method: &str, params: &Value) -> Result<Value, BrokerError> {
        self.owner(params)?;
        if !self.active {
            return Err(BrokerError::fatal(anyhow::anyhow!(
                "callbacks require active turn"
            )));
        }
        match method {
            "fs/read_text_file" | "fs/write_text_file" => {
                if !self.terminals.is_empty() {
                    return Err(BrokerError::recoverable(
                        -32603,
                        "file callbacks require released terminals",
                    ));
                }
                let tool = if method == "fs/read_text_file" {
                    "read_file"
                } else {
                    "write_file"
                };
                if tool == "write_file" {
                    let write_allowed = self
                        .session
                        .assignment
                        .plan
                        .definition
                        .steps
                        .get(&self.session.assignment.step)
                        .and_then(|s| s.agent.as_ref())
                        .is_some_and(|a| a.tools.contains(&"write_file".to_string()));
                    if !write_allowed {
                        return Err(BrokerError::recoverable(
                            -32603,
                            "write operation denied: read-only role workspace",
                        ));
                    }
                }
                let path = match self.path(&params["path"]) {
                    Ok(p) => p,
                    Err(err) => {
                        return Err(BrokerError::recoverable(
                            -32602,
                            format!("invalid ACP path: {err:#}"),
                        ));
                    }
                };

                let root = match crate::acp_files::Root::open(&self.session.workspace.path) {
                    Ok(r) => r,
                    Err(err) => {
                        return Err(BrokerError::fatal(err));
                    }
                };

                let call = self
                    .reserve(Some(tool), params, 0)
                    .await
                    .map_err(BrokerError::from_reserve_error)?;

                let result = if tool == "read_file" {
                    let line = match params.get("line") {
                        Some(v) if !v.is_null() => match v.as_u64() {
                            Some(n) if n > 0 => n,
                            _ => {
                                let err_val = json!({"error": "invalid ACP line"});
                                let _ = self.finish(&call, &err_val).await;
                                return Err(BrokerError::recoverable(
                                    -32602,
                                    "invalid ACP line: line must be a positive integer",
                                ));
                            }
                        },
                        _ => 1,
                    };
                    let limit = match params.get("limit") {
                        Some(v) if !v.is_null() => match v.as_u64() {
                            Some(n) if n > 0 && n <= crate::acp_files::MAX_LINE_LIMIT => n,
                            _ => {
                                let err_val = json!({"error": "invalid ACP line limit"});
                                let _ = self.finish(&call, &err_val).await;
                                return Err(BrokerError::recoverable(
                                    -32602,
                                    "invalid ACP line limit: limit must be between 1 and 65536",
                                ));
                            }
                        },
                        _ => crate::acp_files::MAX_LINE_LIMIT,
                    };

                    match root.read_text_range(
                        &path,
                        line,
                        limit,
                        crate::acp_files::MAX_RESPONSE_BYTES,
                        crate::acp_files::MAX_LINE_BYTES,
                    ) {
                        Ok(text) => json!({"content": text}),
                        Err(err) => {
                            let err_msg = format!("{err:#}");
                            let err_val = json!({"error": &err_msg});
                            let _ = self.finish(&call, &err_val).await;
                            return Err(BrokerError::recoverable(-32603, err_msg));
                        }
                    }
                } else {
                    let content = match params.get("content").and_then(|v| v.as_str()) {
                        Some(s) if s.len() <= 65536 => s,
                        Some(_) => {
                            let err_val = json!({"error": "ACP file content bound exceeded"});
                            let _ = self.finish(&call, &err_val).await;
                            return Err(BrokerError::recoverable(
                                -32602,
                                "ACP file content bound exceeded",
                            ));
                        }
                        None => {
                            let err_val =
                                json!({"error": "ACP file content missing or not a string"});
                            let _ = self.finish(&call, &err_val).await;
                            return Err(BrokerError::recoverable(
                                -32602,
                                "ACP file content missing or not a string",
                            ));
                        }
                    };
                    match root.write(&path, content.as_bytes()) {
                        Ok(()) => json!({}),
                        Err(err) => {
                            let err_msg = format!("{err:#}");
                            let err_val = json!({"error": &err_msg});
                            let _ = self.finish(&call, &err_val).await;
                            return Err(BrokerError::recoverable(-32603, err_msg));
                        }
                    }
                };

                self.record(
                    RecordKind::BrokerOutput,
                    &result,
                    serde_json::to_vec(&result)
                        .map(|v| v.len() as u64)
                        .unwrap_or(0),
                    0,
                )
                .await
                .map_err(BrokerError::fatal)?;

                self.finish(&call, &result)
                    .await
                    .map_err(BrokerError::fatal)?;
                Ok(result)
            }
            "terminal/create" => {
                let request: agent_client_protocol::CreateTerminalRequest =
                    match serde_json::from_value(params.clone()) {
                        Ok(req) => req,
                        Err(_) => {
                            return Err(BrokerError::recoverable(
                                -32602,
                                "invalid ACP terminal request",
                            ));
                        }
                    };
                if !self.terminals.is_empty() || !request.env.is_empty() {
                    return Err(BrokerError::recoverable(
                        -32603,
                        "one terminal with no agent environment allowed",
                    ));
                }
                let cwd = match request.cwd {
                    Some(ref p) if p == std::path::Path::new(crate::acp_runtime::WORKSPACE) => {
                        ".".into()
                    }
                    Some(p) => match self.path(&json!(p)) {
                        Ok(p) => p,
                        Err(err) => {
                            return Err(BrokerError::recoverable(
                                -32602,
                                format!("invalid terminal cwd: {err:#}"),
                            ));
                        }
                    },
                    None => ".".into(),
                };
                let command = CommandSpec {
                    argv: std::iter::once(request.command)
                        .chain(request.args)
                        .collect(),
                    cwd,
                    timeout_seconds: self.limits().terminal_timeout_seconds,
                };
                if let Err(err) = crate::execution::validate_tool_command(&command) {
                    return Err(BrokerError::recoverable(
                        -32603,
                        format!("command validation failed: {err:#}"),
                    ));
                }
                let output_limit = request.output_byte_limit.unwrap_or(65536).min(65536) as usize;
                if output_limit == 0 {
                    return Err(BrokerError::recoverable(
                        -32602,
                        "empty ACP output allowance",
                    ));
                }
                let call = self
                    .reserve(Some("shell"), params, command.timeout_seconds)
                    .await
                    .map_err(BrokerError::from_reserve_error)?;
                let spec = match crate::workspace::prepare_execution(
                    self.session.assignment,
                    &self.session.workspace.path,
                    self.session.directory,
                    self.session.profile,
                    &command,
                )
                .await
                {
                    Ok(s) => s,
                    Err(err) => {
                        let err_msg = format!("{err:#}");
                        let err_val = json!({"error": &err_msg});
                        let _ = self.finish(&call, &err_val).await;
                        return Err(BrokerError::recoverable(-32603, err_msg));
                    }
                };
                let terminal = match Terminal::start(
                    &spec,
                    self.session.directory,
                    output_limit,
                    self.limits().output_bytes.saturating_sub(self.output_bytes),
                )
                .await
                {
                    Ok(t) => t,
                    Err(err) => {
                        let err_msg = format!("{err:#}");
                        let err_val = json!({"error": &err_msg});
                        let _ = self.finish(&call, &err_val).await;
                        return Err(BrokerError::recoverable(-32603, err_msg));
                    }
                };
                let id = id();
                self.terminals.insert(
                    id.clone(),
                    Handle {
                        terminal: Rc::new(terminal),
                        call,
                        finished: false,
                    },
                );
                Ok(json!({"terminalId": id}))
            }
            "terminal/output" | "terminal/wait_for_exit" | "terminal/kill" | "terminal/release" => {
                let id = match params.get("terminalId").and_then(|v| v.as_str()) {
                    Some(id) => id,
                    None => {
                        return Err(BrokerError::recoverable(-32602, "terminal ID missing"));
                    }
                };
                let terminal = match self.terminals.get(id) {
                    Some(h) => h.terminal.clone(),
                    None => {
                        return Err(BrokerError::recoverable(
                            -32602,
                            "foreign or released terminal",
                        ));
                    }
                };
                let output = match method {
                    "terminal/output" => terminal.output(),
                    "terminal/wait_for_exit" => match terminal.wait().await {
                        Ok(out) => out,
                        Err(err) => {
                            return Err(BrokerError::recoverable(-32603, format!("{err:#}")));
                        }
                    },
                    _ => match terminal.kill().await {
                        Ok(out) => out,
                        Err(err) => {
                            return Err(BrokerError::recoverable(-32603, format!("{err:#}")));
                        }
                    },
                };
                let exit = output.exit_code.map(|code| json!({"exitCode": code}));
                let result = match method {
                    "terminal/output" => {
                        let mut value =
                            json!({"output": output.text(), "truncated": output.truncated});
                        if let Some(exit) = &exit {
                            value["exitStatus"] = exit.clone();
                        }
                        value
                    }
                    "terminal/wait_for_exit" => match exit {
                        Some(exit) => json!({"exitStatus": exit}),
                        None => {
                            return Err(BrokerError::recoverable(
                                -32603,
                                "terminal exit unavailable",
                            ));
                        }
                    },
                    _ => json!({}),
                };
                if output.complete && !self.terminals[id].finished {
                    if !output.cleanup_confirmed || output.overflow {
                        return Err(BrokerError::fatal(anyhow::anyhow!(
                            "terminal cleanup or output unconfirmed"
                        )));
                    }
                    let receipt = json!({
                        "exit_code": output.exit_code,
                        "output_digest": digest(&output.bytes),
                        "total_bytes": output.total,
                        "cleanup_confirmed": true,
                    });
                    self.record(RecordKind::BrokerOutput, &receipt, output.total, 0)
                        .await
                        .map_err(BrokerError::fatal)?;
                    let call = self.terminals[id].call.clone();
                    self.finish(&call, &receipt)
                        .await
                        .map_err(BrokerError::fatal)?;
                    self.terminals.get_mut(id).unwrap().finished = true;
                }
                if method == "terminal/release" {
                    self.terminals.remove(id);
                }
                Ok(result)
            }
            // Native approval is not a broker dispatch. No implicit auth, MCP,
            // extensions, URL opening or delegated process execution is permitted.
            _ => Err(BrokerError::recoverable(
                -32601,
                "unsupported ACP client operation",
            )),
        }
    }
    pub async fn close_terminals(&mut self) -> Result<()> {
        for handle in self.terminals.values() {
            let _ = handle.terminal.kill().await;
        }
        self.terminals.clear();
        Ok(())
    }
}
