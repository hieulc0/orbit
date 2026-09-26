//! Version-pinned App Server ↔ ACP session adapter. Native effects are disabled;
//! only dynamic functions can request client-owned repository effects.
use crate::{
    acp_process::Request,
    acp_wire::Wire,
    codex_bridge::{ToolCall, ToolRouter, thread_start},
};
use agent_client_protocol as acp;
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::Path;
use tokio::sync::Mutex;

/// Bounded protocol state for cleanup evidence. Every string is a fixed local
/// category; peer-controlled method names, IDs and payloads are never retained.
#[derive(Debug, Clone, Serialize)]
pub struct SessionDiagnostics {
    pub(crate) phase: &'static str,
    pub(crate) last_activity: &'static str,
    pub(crate) pending_request: Option<&'static str>,
    pub(crate) outcome: &'static str,
    pub(crate) turn_outcome: &'static str,
    pub(crate) server_request_count: u8,
    pub(crate) peer_eof_observed: bool,
    pub(crate) app_server_stdout_eof_observed: bool,
}

impl Default for SessionDiagnostics {
    fn default() -> Self {
        Self {
            phase: "bridge_start",
            last_activity: "none",
            pending_request: None,
            outcome: "running",
            turn_outcome: "not_started",
            server_request_count: 0,
            peer_eof_observed: false,
            app_server_stdout_eof_observed: false,
        }
    }
}

impl SessionDiagnostics {
    fn request_sent(&mut self, method: &str) {
        let (phase, category) = match method {
            "initialize" => ("initializing", "initialize"),
            "account/read" => ("session_setup", "account_read"),
            "thread/start" => ("session_setup", "thread_start"),
            "turn/start" => ("turn_start", "turn_start"),
            "turn/interrupt" => ("cancelling", "turn_interrupt"),
            _ => ("protocol", "other_request"),
        };
        self.phase = phase;
        self.last_activity = "app_server_request_sent";
        self.pending_request = Some(category);
    }

    fn server_message(&mut self, message: &Value) {
        if message.get("id").is_some() {
            self.server_request_count = self.server_request_count.saturating_add(1).min(64);
            self.last_activity = if message["method"] == "item/tool/call" {
                "dynamic_tool_request_received"
            } else {
                "server_request_received"
            };
            return;
        }
        self.last_activity = match message["method"].as_str() {
            Some("error") => "server_error_notification",
            Some("turn/started") => "turn_started_notification",
            Some("turn/completed") => "turn_completed_notification",
            Some("item/started" | "item/completed") => "item_lifecycle_notification",
            Some("item/agentMessage/delta") => "agent_message_notification",
            Some("warning" | "configWarning") => "server_warning_notification",
            _ => "server_notification",
        };
    }

    fn app_server_read_error(&mut self, error: &anyhow::Error) {
        self.last_activity = if error
            .downcast_ref::<crate::acp_wire::StreamClosed>()
            .is_some()
        {
            "app_server_stdout_eof"
        } else {
            "app_server_stdout_read_error"
        };
        self.outcome = if self.last_activity == "app_server_stdout_eof" {
            "app_server_eof"
        } else {
            "protocol_read_failure"
        };
        self.app_server_stdout_eof_observed = self.last_activity == "app_server_stdout_eof";
    }

    fn peer_read_error(&mut self, error: &anyhow::Error) {
        self.last_activity = if error
            .downcast_ref::<crate::acp_wire::StreamClosed>()
            .is_some()
        {
            "acp_peer_eof"
        } else {
            "acp_peer_read_error"
        };
        self.peer_eof_observed = self.last_activity == "acp_peer_eof";
        self.outcome = if self.peer_eof_observed && self.turn_outcome == "end_turn" {
            "peer_eof_after_end_turn"
        } else if self.peer_eof_observed {
            "peer_eof"
        } else {
            "protocol_read_failure"
        };
    }

    fn correlation_failure(&mut self) {
        self.last_activity = "response_correlation_failure";
        self.outcome = "correlation_failure";
    }

    fn protocol_rejection(&mut self) {
        self.last_activity = "correlated_protocol_rejection";
        self.outcome = "protocol_rejection";
        self.pending_request = None;
    }
}

struct Client(Mutex<Wire>);
impl Client {
    async fn call<T: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: T,
    ) -> acp::Result<R> {
        let result = async {
            let mut wire = self.0.lock().await;
            let id = wire.request(method, serde_json::to_value(params)?).await?;
            let value = wire.read().await?;
            // A concurrent cancel/EOF interrupts the pending callback. The process
            // supervisor then removes Codex; the worker independently stops tools.
            serde_json::from_value(Wire::result(value, &id)?)
                .context("invalid ACP callback response")
        }
        .await;
        result.map_err(|_| acp::Error::internal_error())
    }
}
#[async_trait::async_trait(?Send)]
impl acp::Client for Client {
    async fn request_permission(
        &self,
        _: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        Err(acp::Error::method_not_found())
    }
    async fn session_notification(&self, p: acp::SessionNotification) -> acp::Result<()> {
        self.0
            .lock()
            .await
            .notify(
                "session/update",
                serde_json::to_value(p).map_err(|_| acp::Error::internal_error())?,
            )
            .await
            .map_err(|_| acp::Error::internal_error())
    }
    async fn read_text_file(
        &self,
        p: acp::ReadTextFileRequest,
    ) -> acp::Result<acp::ReadTextFileResponse> {
        self.call("fs/read_text_file", p).await
    }
    async fn write_text_file(
        &self,
        p: acp::WriteTextFileRequest,
    ) -> acp::Result<acp::WriteTextFileResponse> {
        self.call("fs/write_text_file", p).await
    }
    async fn create_terminal(
        &self,
        p: acp::CreateTerminalRequest,
    ) -> acp::Result<acp::CreateTerminalResponse> {
        self.call("terminal/create", p).await
    }
    async fn wait_for_terminal_exit(
        &self,
        p: acp::WaitForTerminalExitRequest,
    ) -> acp::Result<acp::WaitForTerminalExitResponse> {
        self.call("terminal/wait_for_exit", p).await
    }
    async fn terminal_output(
        &self,
        p: acp::TerminalOutputRequest,
    ) -> acp::Result<acp::TerminalOutputResponse> {
        self.call("terminal/output", p).await
    }
    async fn release_terminal(
        &self,
        p: acp::ReleaseTerminalRequest,
    ) -> acp::Result<acp::ReleaseTerminalResponse> {
        self.call("terminal/release", p).await
    }
}

#[async_trait::async_trait(?Send)]
impl crate::codex_bridge::OrbitAcpClient for Client {
    async fn create_directory(&self, path: &Path, recursive: bool) -> Result<String> {
        let res: serde_json::Value = self
            .call(
                "fs/create_directory",
                serde_json::json!({
                    "path": path.to_string_lossy(),
                    "recursive": recursive,
                }),
            )
            .await
            .map_err(|e| anyhow::anyhow!("ACP create_directory failed: {e}"))?;
        if let Some(err) = res.get("error").and_then(|e| e.as_str()) {
            Ok(format!("Error: {err}"))
        } else {
            Ok(res
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("Directory created successfully.")
                .to_string())
        }
    }

    async fn move_path(&self, source: &Path, destination: &Path) -> Result<String> {
        let res: serde_json::Value = self
            .call(
                "fs/move",
                serde_json::json!({
                    "source": source.to_string_lossy(),
                    "destination": destination.to_string_lossy(),
                }),
            )
            .await
            .map_err(|e| anyhow::anyhow!("ACP move failed: {e}"))?;
        if let Some(err) = res.get("error").and_then(|e| e.as_str()) {
            Ok(format!("Error: {err}"))
        } else {
            Ok(res
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("Moved successfully.")
                .to_string())
        }
    }

    async fn delete_file(&self, path: &Path) -> Result<String> {
        let res: serde_json::Value = self
            .call(
                "fs/delete_file",
                serde_json::json!({
                    "path": path.to_string_lossy(),
                }),
            )
            .await
            .map_err(|e| anyhow::anyhow!("ACP delete_file failed: {e}"))?;
        if let Some(err) = res.get("error").and_then(|e| e.as_str()) {
            Ok(format!("Error: {err}"))
        } else {
            Ok(res
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("File deleted successfully.")
                .to_string())
        }
    }

    async fn delete_directory(&self, path: &Path, recursive: bool) -> Result<String> {
        let res: serde_json::Value = self
            .call(
                "fs/delete_directory",
                serde_json::json!({
                    "path": path.to_string_lossy(),
                    "recursive": recursive,
                }),
            )
            .await
            .map_err(|e| anyhow::anyhow!("ACP delete_directory failed: {e}"))?;
        if let Some(err) = res.get("error").and_then(|e| e.as_str()) {
            Ok(format!("Error: {err}"))
        } else {
            Ok(res
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("Directory deleted successfully.")
                .to_string())
        }
    }
}

async fn control(
    server: &mut Wire,
    method: &str,
    params: Value,
    diagnostics: &mut SessionDiagnostics,
) -> Result<Value> {
    diagnostics.request_sent(method);
    let id = server.request(method, params).await?;
    loop {
        let message = match server.read().await {
            Ok(message) => message,
            Err(error) => {
                diagnostics.app_server_read_error(&error);
                return Err(error).context("Codex App Server read failed");
            }
        };
        if message.get("method").is_none() {
            if message.get("id") != Some(&id) {
                diagnostics.correlation_failure();
                anyhow::bail!("Codex App Server response ID mismatch");
            }
            let result = Wire::result(message, &id);
            if result.is_err() {
                diagnostics.protocol_rejection();
            } else {
                diagnostics.pending_request = None;
                diagnostics.last_activity = "app_server_response_received";
            }
            return result;
        }
        diagnostics.server_message(&message);
        if message.get("id").is_some() {
            diagnostics.outcome = "server_request_rejected";
            anyhow::bail!("unexpected Codex server request during setup");
        }
        if message["method"] == "error" {
            diagnostics.outcome = "app_server_error_notification";
            anyhow::bail!("Codex setup failed");
        }
    }
}

pub async fn run(
    request: &Request,
    mut server: Wire,
    client: Wire,
    diagnostics: &mut SessionDiagnostics,
) -> Result<()> {
    let client = Client(Mutex::new(client));
    let mut initialized = false;
    let mut session: Option<(String, String, std::path::PathBuf)> = None;
    let mut prompted = false;
    let mut seen = std::collections::BTreeSet::new();
    loop {
        let message = match client.0.lock().await.read().await {
            Ok(message) => message,
            Err(error) => {
                diagnostics.peer_read_error(&error);
                return Err(error).context("Codex ACP peer read failed");
            }
        };
        diagnostics.phase = "bridge_request";
        diagnostics.last_activity = "acp_request_received";
        let id = message
            .get("id")
            .cloned()
            .context("ACP bridge expects a request")?;
        ensure!(
            seen.len() < 64 && seen.insert(serde_json::to_string(&id)?),
            "duplicate bridge request"
        );
        let params = &message["params"];
        let result = match message["method"].as_str() {
            Some("initialize") => {
                ensure!(
                    !initialized && params["protocolVersion"] == 1,
                    "unsupported ACP initialize"
                );
                let response = control(
                    &mut server,
                    "initialize",
                    json!({"clientInfo":{"name":"orbit","version":"1"},
                    "capabilities":{"experimentalApi":true}}),
                    diagnostics,
                )
                .await?;
                ensure!(
                    response["codexHome"] == "/orbit/home/.codex",
                    "Codex control HOME mismatch"
                );
                server.notify("initialized", json!({})).await?;
                diagnostics.last_activity = "initialized_notification_sent";
                initialized = true;
                json!({"protocolVersion":1,"agentInfo":{"name":"orbit-codex-acp","version":"1"},"agentCapabilities":{},"authMethods":[]})
            }
            Some("session/new") => {
                ensure!(
                    initialized && session.is_none() && params["mcpServers"] == json!([]),
                    "bridge sessions are new and have no MCP servers"
                );
                let workspace = std::path::PathBuf::from(
                    params["cwd"].as_str().context("ACP workspace missing")?,
                );
                ensure!(workspace.is_absolute(), "ACP workspace must be absolute");
                let account = control(
                    &mut server,
                    "account/read",
                    json!({"refreshToken":false}),
                    diagnostics,
                )
                .await?;
                ensure!(
                    !account["account"].is_null() || account["requiresOpenaiAuth"] == false,
                    "Codex authentication required; provision selected account outside workflow"
                );
                let model = request
                    .runtime
                    .binding
                    .model
                    .as_ref()
                    .context("Codex model pin missing")?;
                let created = control(
                    &mut server,
                    "thread/start",
                    thread_start(
                        &request.runtime.launch.binary_revision,
                        model,
                        request.runtime.reasoning_effort.as_deref(),
                        std::path::Path::new("/orbit/home"),
                        &request.tools,
                    )?,
                    diagnostics,
                )
                .await?;
                ensure!(created["model"] == *model, "Codex model mismatch");
                let actual_reasoning_effort =
                    created.get("reasoningEffort").and_then(Value::as_str);
                if let Some(requested) = request.runtime.reasoning_effort.as_deref() {
                    ensure!(
                        actual_reasoning_effort == Some(requested),
                        "Codex reasoning effort mismatch"
                    );
                }
                diagnostics.phase = "session_ready";
                diagnostics.last_activity = "session_created";
                let thread = created["thread"]["id"]
                    .as_str()
                    .context("Codex thread identity missing")?;
                ensure!(crate::agent::valid_name(thread), "invalid Codex thread");
                let session_id = crate::model::id();
                session = Some((session_id.clone(), thread.into(), workspace));
                let mut response = json!({"sessionId":session_id,"models":{"currentModelId":model,"availableModels":[{"modelId":model,"name":model}]}});
                if let Some(effort) = actual_reasoning_effort {
                    response["_meta"] = json!({"orbit":{"codexReasoningEffort":effort}});
                }
                response
            }
            Some("session/prompt") => {
                let (session_id, thread, workspace) =
                    session.as_ref().context("Codex session missing")?;
                ensure!(
                    !prompted && params["sessionId"] == *session_id,
                    "bridge accepts one prompt per new session"
                );
                prompted = true;
                diagnostics.phase = "prompt_received";
                diagnostics.last_activity = "prompt_received";
                let prompt = params["prompt"].as_array().context("text prompt missing")?;
                ensure!(
                    (1..=16).contains(&prompt.len())
                        && prompt
                            .iter()
                            .all(|p| p["type"] == "text" && p["text"].is_string()),
                    "bridge supports text prompts only"
                );
                let input = prompt
                    .iter()
                    .map(|p| json!({"type":"text","text":p["text"],"text_elements":[]}))
                    .collect::<Vec<_>>();
                let active = ActiveSession {
                    session: session_id,
                    thread,
                    workspace,
                };
                turn(request, &mut server, &client, active, input, diagnostics).await?
            }
            _ => {
                client
                    .0
                    .lock()
                    .await
                    .response(id, Err(anyhow::anyhow!("unsupported bridge operation")))
                    .await?;
                diagnostics.last_activity = "acp_request_rejected";
                continue;
            }
        };
        client.0.lock().await.response(id, Ok(result)).await?;
        if diagnostics.outcome == "end_turn" {
            diagnostics.last_activity = "acp_end_turn_response_sent";
        }
    }
}

struct ActiveSession<'a> {
    session: &'a str,
    thread: &'a str,
    workspace: &'a std::path::Path,
}

async fn turn(
    request: &Request,
    server: &mut Wire,
    client: &Client,
    active: ActiveSession<'_>,
    input: Vec<Value>,
    diagnostics: &mut SessionDiagnostics,
) -> Result<Value> {
    let ActiveSession {
        session,
        thread,
        workspace,
    } = active;
    diagnostics.phase = "turn_start";
    diagnostics.pending_request = Some("turn_start");
    diagnostics.last_activity = "app_server_request_sent";
    diagnostics.turn_outcome = "start_pending";
    let request_id = server
        .request("turn/start", json!({"threadId":thread,"input":input}))
        .await?;
    let mut turn_id: Option<String> = None;
    let mut router = None;
    let mut acknowledged = false;
    let mut requests = std::collections::BTreeSet::new();
    loop {
        let incoming = {
            let mut wire = client.0.lock().await;
            tokio::select! {
                result=server.read()=> (true, result),
                result=wire.read()=> (false, result),
            }
        };
        let (from_server, result) = incoming;
        let message = match result {
            Ok(message) => message,
            Err(error) => {
                if from_server {
                    diagnostics.app_server_read_error(&error);
                    return Err(error).context("Codex App Server turn stream read failed");
                }
                diagnostics.peer_read_error(&error);
                return Err(error).context("Codex ACP peer turn read failed");
            }
        };
        if !from_server {
            ensure!(
                message["method"] == "session/cancel" && message["params"]["sessionId"] == session,
                "unexpected ACP request during prompt"
            );
            diagnostics.phase = "cancelling";
            diagnostics.last_activity = "acp_cancel_received";
            diagnostics.outcome = "cancelled";
            if let Some(turn) = &turn_id {
                let _ = server
                    .request("turn/interrupt", json!({"threadId":thread,"turnId":turn}))
                    .await;
            }
            // Returning closes the ACP prompt; the supervisor is then
            // stopped by the worker and confirms complete container removal.
            return Ok(json!({"stopReason":"cancelled"}));
        }
        if message.get("method").is_none() {
            ensure!(!acknowledged, "duplicate Codex turn response");
            if message.get("id") != Some(&request_id) {
                diagnostics.correlation_failure();
                anyhow::bail!("Codex turn response ID mismatch");
            }
            let result = Wire::result(message, &request_id).inspect_err(|_error| {
                diagnostics.protocol_rejection();
            })?;
            let id = result["turn"]["id"]
                .as_str()
                .context("Codex turn identity missing")?;
            ensure!(
                turn_id.as_deref().is_none_or(|old| old == id),
                "Codex turn identity changed"
            );
            turn_id = Some(id.into());
            acknowledged = true;
            diagnostics.pending_request = None;
            diagnostics.phase = "turn_running";
            diagnostics.last_activity = "turn_start_response_received";
            diagnostics.turn_outcome = "running";
            continue;
        }
        diagnostics.server_message(&message);
        let method = message["method"].as_str().context("Codex method missing")?;
        let params = &message["params"];
        if let Some(id) = message.get("id") {
            ensure!(
                requests.len() < 1024 && requests.insert(serde_json::to_string(id)?),
                "duplicate Codex reverse request"
            );
            if method != "item/tool/call" {
                diagnostics.outcome = "server_request_rejected";
                anyhow::bail!("unexpected Codex server request during turn");
            }
            diagnostics.last_activity = "dynamic_tool_request_received";
            let turn = turn_id
                .as_deref()
                .context("Codex tool before turn identity")?;
            if router.is_none() {
                router = Some(ToolRouter::new(
                    session,
                    thread,
                    turn,
                    workspace,
                    &request.tools,
                    request
                        .runtime
                        .binding
                        .acp
                        .as_ref()
                        .unwrap()
                        .max_limits
                        .broker_calls as usize,
                )?);
            }
            let call: ToolCall = serde_json::from_value(params.clone())
                .map_err(|_| anyhow::anyhow!("invalid Codex tool call"))?;
            client.0.lock().await.notify("session/update",json!({"sessionId":session,"update":{
                "sessionUpdate":"tool_call","toolCallId":call.call_id,"title":call.tool,"kind":"other","status":"in_progress"}})).await?;
            let result = router.as_mut().unwrap().dispatch(client, call).await;
            let failed = result.is_err();
            server.response(id.clone(), result).await?;
            if failed {
                diagnostics.outcome = "tool_callback_failed";
                anyhow::bail!("Codex broker callback failed");
            }
            diagnostics.last_activity = "dynamic_tool_result_sent";
        } else {
            if let Some(owner) = params.get("threadId") {
                ensure!(owner == thread, "foreign Codex thread notification");
            }
            if let Some(owner) = params.get("turnId") {
                ensure!(
                    turn_id.as_deref().is_some_and(|turn| owner == turn),
                    "foreign Codex turn notification"
                );
            }
            match method {
                "turn/started" => {
                    diagnostics.phase = "turn_running";
                    diagnostics.last_activity = "turn_started_notification";
                    let id = params["turn"]["id"]
                        .as_str()
                        .context("Codex turn notification missing identity")?;
                    ensure!(
                        turn_id.as_deref().is_none_or(|old| old == id),
                        "Codex turn notification mismatch"
                    );
                    turn_id = Some(id.into());
                    diagnostics.turn_outcome = "running";
                }
                "turn/completed" => {
                    if !(acknowledged
                        && params["turn"]["id"].as_str() == turn_id.as_deref()
                        && params["turn"]["status"] == "completed")
                    {
                        diagnostics.outcome = "turn_incomplete";
                        diagnostics.last_activity = "turn_completed_not_successful";
                        anyhow::bail!("Codex turn incomplete");
                    }
                    diagnostics.phase = "turn_completed";
                    diagnostics.last_activity = "turn_completed_notification";
                    diagnostics.outcome = "end_turn";
                    diagnostics.turn_outcome = "end_turn";
                    return Ok(json!({"stopReason":"end_turn"}));
                }
                "item/started" | "item/completed" => {
                    ensure!(
                        [
                            "userMessage",
                            "agentMessage",
                            "reasoning",
                            "plan",
                            "dynamicToolCall"
                        ]
                        .contains(&params["item"]["type"].as_str().unwrap_or("")),
                        "unmediated Codex item denied"
                    );
                }
                "item/agentMessage/delta" => {
                    let delta = params["delta"]
                        .as_str()
                        .context("invalid Codex text delta")?;
                    client.0.lock().await.notify("session/update",json!({"sessionId":session,
                        "update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":delta}}})).await?;
                }
                "error" => {
                    diagnostics.outcome = "app_server_error_notification";
                    diagnostics.turn_outcome = "app_server_error_notification";
                    anyhow::bail!("Codex turn failed: {params:?}")
                }
                _ => {} // bounded control telemetry; never raw-reasoning persistence
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SessionDiagnostics;
    use serde_json::json;

    #[test]
    fn lifecycle_evidence_uses_fixed_categories_and_preserves_turn_result() -> anyhow::Result<()> {
        let mut diagnostics = SessionDiagnostics::default();
        diagnostics.server_message(&json!({"method":"error","params":{"message":"prompt-secret"}}));
        assert_eq!(diagnostics.last_activity, "server_error_notification");
        diagnostics.turn_outcome = "end_turn";
        diagnostics.peer_read_error(&crate::acp_wire::StreamClosed.into());
        assert_eq!(diagnostics.outcome, "peer_eof_after_end_turn");
        let serialized = serde_json::to_string(&diagnostics)?;
        assert!(!serialized.contains("prompt-secret"));
        assert!(!serialized.contains("\"method\""));
        assert!(serialized.len() <= 1024);
        Ok(())
    }
}
