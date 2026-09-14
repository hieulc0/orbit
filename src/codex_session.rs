//! Version-pinned App Server ↔ ACP session adapter. Native effects are disabled;
//! only dynamic functions can request client-owned repository effects.
use crate::{
    acp_process::Request,
    acp_wire::Wire,
    codex_bridge::{ToolCall, ToolRouter, thread_start},
};
use agent_client_protocol as acp;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use tokio::sync::Mutex;

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

async fn control(server: &mut Wire, method: &str, params: Value) -> Result<Value> {
    let id = server.request(method, params).await?;
    loop {
        let message = server.read().await?;
        if message.get("method").is_none() {
            return Wire::result(message, &id);
        }
        ensure!(
            message.get("id").is_none(),
            "unexpected Codex reverse request before turn"
        );
        if message["method"] == "error" {
            anyhow::bail!("Codex setup failed");
        }
    }
}

pub async fn run(request: &Request, mut server: Wire, client: Wire) -> Result<()> {
    let client = Client(Mutex::new(client));
    let mut initialized = false;
    let mut session: Option<(String, String, std::path::PathBuf)> = None;
    let mut prompted = false;
    let mut seen = std::collections::BTreeSet::new();
    loop {
        let message = client.0.lock().await.read().await?;
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
                )
                .await?;
                ensure!(
                    response["codexHome"] == "/orbit/home/.codex",
                    "Codex control HOME mismatch"
                );
                server.notify("initialized", json!({})).await?;
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
                let account =
                    control(&mut server, "account/read", json!({"refreshToken":false})).await?;
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
                        std::path::Path::new("/orbit/home"),
                        &request.tools,
                    )?,
                )
                .await?;
                ensure!(created["model"] == *model, "Codex model mismatch");
                let thread = created["thread"]["id"]
                    .as_str()
                    .context("Codex thread identity missing")?;
                ensure!(crate::agent::valid_name(thread), "invalid Codex thread");
                let session_id = crate::model::id();
                session = Some((session_id.clone(), thread.into(), workspace));
                json!({"sessionId":session_id,"models":{"currentModelId":model,"availableModels":[{"modelId":model,"name":model}]}})
            }
            Some("session/prompt") => {
                let (session_id, thread, workspace) =
                    session.as_ref().context("Codex session missing")?;
                ensure!(
                    !prompted && params["sessionId"] == *session_id,
                    "bridge accepts one prompt per new session"
                );
                prompted = true;
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
                turn(
                    request,
                    &mut server,
                    &client,
                    session_id,
                    thread,
                    workspace,
                    input,
                )
                .await?
            }
            _ => {
                client
                    .0
                    .lock()
                    .await
                    .response(id, Err(anyhow::anyhow!("unsupported bridge operation")))
                    .await?;
                continue;
            }
        };
        client.0.lock().await.response(id, Ok(result)).await?;
    }
}

async fn turn(
    request: &Request,
    server: &mut Wire,
    client: &Client,
    session: &str,
    thread: &str,
    workspace: &std::path::Path,
    input: Vec<Value>,
) -> Result<Value> {
    let request_id = server
        .request("turn/start", json!({"threadId":thread,"input":input}))
        .await?;
    let mut turn_id: Option<String> = None;
    let mut router = None;
    let mut acknowledged = false;
    let mut requests = std::collections::BTreeSet::new();
    loop {
        let message = {
            let mut wire = client.0.lock().await;
            tokio::select! {
                result=server.read()=>result?,
                result=wire.read()=>{
                    let message=result?;
                    ensure!(message["method"] == "session/cancel" && message["params"]["sessionId"] == session,"unexpected ACP request during prompt");
                    if let Some(turn)=&turn_id {let _=server.request("turn/interrupt",json!({"threadId":thread,"turnId":turn})).await;}
                    // Returning closes the ACP prompt; the supervisor is then
                    // stopped by the worker and confirms complete container removal.
                    return Ok(json!({"stopReason":"cancelled"}));
                }
            }
        };
        if message.get("method").is_none() {
            ensure!(!acknowledged, "duplicate Codex turn response");
            let result = Wire::result(message, &request_id)?;
            let id = result["turn"]["id"]
                .as_str()
                .context("Codex turn identity missing")?;
            ensure!(
                turn_id.as_deref().is_none_or(|old| old == id),
                "Codex turn identity changed"
            );
            turn_id = Some(id.into());
            acknowledged = true;
            continue;
        }
        let method = message["method"].as_str().context("Codex method missing")?;
        let params = &message["params"];
        if let Some(id) = message.get("id") {
            ensure!(
                requests.len() < 1024 && requests.insert(serde_json::to_string(id)?),
                "duplicate Codex reverse request"
            );
            ensure!(method == "item/tool/call", "native Codex request denied");
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
            ensure!(!failed, "Codex broker call failed");
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
                    let id = params["turn"]["id"]
                        .as_str()
                        .context("Codex turn notification missing identity")?;
                    ensure!(
                        turn_id.as_deref().is_none_or(|old| old == id),
                        "Codex turn notification mismatch"
                    );
                    turn_id = Some(id.into());
                }
                "turn/completed" => {
                    ensure!(
                        acknowledged
                            && params["turn"]["id"].as_str() == turn_id.as_deref()
                            && params["turn"]["status"] == "completed",
                        "Codex turn incomplete"
                    );
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
                "error" => anyhow::bail!("Codex turn failed"),
                _ => {} // bounded control telemetry; never raw-reasoning persistence
            }
        }
    }
}
