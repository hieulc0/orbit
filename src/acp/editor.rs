//! ACP v1 stdio presentation and editor actions. Workflow decisions remain in
//! the coordinator; the wire exposes no implementer filesystem callbacks.
use crate::acp::{service::EditorService, wire::Wire};
use crate::workflow::flow::Skill;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};

fn modes(service: &EditorService, current: &str) -> Value {
    let skills = [
        Skill::Investigate,
        Skill::FixBug,
        Skill::ImplementFeature,
        Skill::Refactor,
        Skill::Review,
        Skill::UpdateDocumentation,
        Skill::DependencyUpdate,
        Skill::ReleasePreparation,
        Skill::SecurityReview,
    ];
    let mut available = Vec::new();
    if service.config().skill.is_none() {
        available.push(json!({"id":"auto","name":"Choose flow from task"}));
    }
    for skill in skills {
        if service
            .config()
            .skill
            .is_none_or(|required| required == skill)
        {
            let name = serde_json::to_value(skill)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned();
            available.push(json!({"id":name,"name":name.replace('_', " ")}));
        }
    }
    json!({"currentModeId":current,"availableModes":available})
}

fn commands() -> Value {
    let entries = [
        (
            "status",
            "Show task, flow, roles, budgets, quota and cleanup",
        ),
        ("diff", "View candidate changes"),
        ("open", "Open the managed attempt path"),
        ("continue", "Continue feedback until review is ready"),
        ("review", "Request technical review and final verification"),
        ("cancel", "Cancel the workflow"),
        (
            "apply",
            "Apply the accepted candidate; pass its WorkspaceStateId",
        ),
        (
            "discard",
            "Discard the candidate; pass its WorkspaceStateId",
        ),
    ];
    json!({"sessionUpdate":"available_commands_update","availableCommands":entries.iter().map(|(name,description)| json!({"name":name,"description":description,"input":{"hint":if matches!(*name,"apply"|"discard") {"WorkspaceStateId"} else {""}}})).collect::<Vec<_>>()})
}

async fn update(
    service: &EditorService,
    output: &mut Wire,
    session_id: &str,
    update: Value,
) -> Result<()> {
    let params = json!({"sessionId":session_id,"update":update});
    service.record_notification(session_id, &params).await?;
    output.notify("session/update", params).await
}

async fn text(
    service: &EditorService,
    output: &mut Wire,
    session_id: &str,
    text: &str,
    user: bool,
) -> Result<()> {
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + 4096).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        update(service, output, session_id, json!({"sessionUpdate":if user {"user_message_chunk"} else {"agent_message_chunk"},"content":{"type":"text","text":&text[start..end]}})).await?;
        start = end;
    }
    Ok(())
}

pub fn render_dashboard(dashboard: &Value) -> String {
    let workflow = &dashboard["workflow"];
    let session = &dashboard["session"];
    let status = workflow["status"].as_str().unwrap_or("ready");
    let latest_role = dashboard["roles"].as_array().and_then(|roles| roles.last());
    let target = latest_role.map(|role| &role["resolved_target"]);
    let label = |value: Option<&Value>, field: &str| {
        value
            .and_then(|value| value[field].as_str())
            .unwrap_or("unknown")
            .replace(['\n', '\r', '|', '`'], " ")
    };
    let execution = dashboard["agent_executions"]
        .as_array()
        .and_then(|items| items.last());
    let budget = execution.map(|item| &item["budget"]);
    let calls = budget
        .and_then(|value| value["usage"]["total_calls"].as_u64())
        .unwrap_or(0);
    let max_calls = budget
        .and_then(|value| value["limits"]["max_total_calls"].as_u64())
        .unwrap_or(0);
    let reads = budget
        .and_then(|value| value["usage"]["file_read_bytes"].as_u64())
        .unwrap_or(0);
    let max_reads = budget
        .and_then(|value| value["limits"]["max_file_read_bytes"].as_u64())
        .unwrap_or(0);
    let flow = &dashboard["flow"];
    let mut panel = format!(
        "\n## Orbit task\n\n| State | Value |\n| --- | --- |\n| Task | `{}` |\n| Workflow | `{}` |\n| Flow / status | {} / {} |\n| Role | {} |\n| Provider / model / account | {} / {} / {} |\n| Repository calls | {} / {} |\n| File read bytes | {} / {} |\n| Candidate | `{}` |\n| Cleanup / execution | {} / {} |\n\n",
        workflow["task_id"].as_str().unwrap_or("not started"),
        session["workflow_run_id"].as_str().unwrap_or("not started"),
        flow["skill"].as_str().unwrap_or("automatic"),
        status,
        label(latest_role, "role_id"),
        label(target, "provider"),
        label(target, "resolved_model"),
        label(target, "credential_id"),
        calls,
        max_calls,
        reads,
        max_reads,
        dashboard["candidate"]["state_id"]
            .as_str()
            .unwrap_or("unavailable"),
        execution
            .and_then(|value| value["cleanup_confirmed"].as_bool())
            .map(|confirmed| if confirmed {
                "confirmed"
            } else {
                "unconfirmed"
            })
            .unwrap_or("unknown"),
        session["state"].as_str().unwrap_or("unknown")
    );
    if let Some(budget) = budget {
        if budget["usage"]["exhausted"] == true {
            panel.push_str(
                "\nTOOL_BUDGET_EXHAUSTED: the role reached its admitted resource ceiling.\n",
            );
        }
        for (name, usage, limit) in [
            ("Mutating callbacks", "mutating_calls", "max_mutating_calls"),
            ("Terminal callbacks", "terminal_calls", "max_terminal_calls"),
            ("Callback output bytes", "output_bytes", "max_output_bytes"),
        ] {
            panel.push_str(&format!(
                "- {name}: {} / {}\n",
                budget["usage"][usage], budget["limits"][limit]
            ));
        }
    }
    if let Some(files) = dashboard["changed_files"]["paths"].as_array() {
        panel.push_str(&format!(
            "\nCandidate changes: {} paths\n",
            dashboard["changed_files"]["total"]
        ));
        for path in files.iter().take(16) {
            let escaped = path
                .as_str()
                .unwrap_or("unknown")
                .replace(['\n', '\r', '`'], " ");
            panel.push_str(&format!(
                "- `{}`\n",
                escaped.chars().take(256).collect::<String>()
            ));
        }
    }
    if let Some(verification) = dashboard["verification"].as_array() {
        for run in verification
            .iter()
            .skip(verification.len().saturating_sub(8))
        {
            panel.push_str(&format!(
                "- Verification {}: {} · `{}`\n",
                run["tier"].as_str().unwrap_or("unknown"),
                run["result"].as_str().unwrap_or("pending"),
                run["workspace_state_id"].as_str().unwrap_or("unknown")
            ));
        }
    }
    if let Some(quotas) = dashboard["quota"]
        .as_array()
        .filter(|quotas| !quotas.is_empty())
    {
        for quota in quotas.iter().take(4) {
            panel.push_str(&format!(
                "\nQuota {}: {} · observed {} ms UTC · expires {} ms UTC\n",
                quota["credential"].as_str().unwrap_or("unknown"),
                quota["freshness"].as_str().unwrap_or("unknown"),
                quota["observed_at_ms"],
                quota["expires_at_ms"]
            ));
            if let Some(windows) = quota["windows"].as_array() {
                for window in windows.iter().take(8) {
                    panel.push_str(&format!(
                        "- {}: {}% remaining; reset {} ms UTC\n",
                        window["label"].as_str().unwrap_or("unknown"),
                        window["remaining_percent"],
                        window["resets_at_ms"]
                    ));
                }
            }
        }
    } else {
        panel.push_str("\nQuota/reset: no observation available.\n");
    }
    panel.push_str("\nActions: `/diff`, `/open`, `/status`, `/continue`, `/review`, `/cancel`, `/apply WorkspaceStateId`, `/discard WorkspaceStateId`. Apply and discard require the displayed candidate identity.\n");
    panel
}

async fn dashboard_update(
    service: &EditorService,
    output: &mut Wire,
    session_id: &str,
) -> Result<()> {
    let dashboard = service.dashboard(session_id).await?;
    let stage = dashboard["workflow"]["status"]
        .as_str()
        .unwrap_or("created");
    let completion = dashboard["flow"]["completion_tier"]
        .as_str()
        .unwrap_or("FULL");
    let entries = ["PLAN", "IMPLEMENT", "VERIFY", "REVIEW", completion];
    let position = match stage {
        "planning" => 0,
        "implementing" | "repairing" => 1,
        "verifying" => 2,
        "reviewing" => 3,
        "regression" => 4,
        "completed" => 5,
        _ => 0,
    };
    let read_only = dashboard["flow"]["read_only"] == true;
    update(service, output, session_id, json!({"sessionUpdate":"plan","entries":entries.iter().enumerate().filter(|(index,_)| !read_only || *index == 0).map(|(index, name)| json!({"content":name,"priority":"medium","status":if position > index {"completed"} else if position == index {"in_progress"} else {"pending"}})).collect::<Vec<_>>()})).await?;
    text(
        service,
        output,
        session_id,
        &render_dashboard(&dashboard),
        false,
    )
    .await
}

fn session_id(params: &Value) -> Result<&str> {
    params["sessionId"]
        .as_str()
        .filter(|id| id.len() <= 128)
        .context("sessionId required")
}

fn prompt_text(params: &Value) -> Result<String> {
    let blocks = params["prompt"]
        .as_array()
        .context("prompt blocks required")?;
    ensure!(
        !blocks.is_empty() && blocks.len() <= 128,
        "invalid prompt blocks"
    );
    let mut text = String::new();
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => text.push_str(block["text"].as_str().context("text block required")?),
            Some("resource_link") => {
                text.push_str("\nAttached resource: ");
                text.push_str(block["uri"].as_str().context("resource URI required")?);
            }
            _ => anyhow::bail!("unsupported ACP prompt content"),
        }
        ensure!(text.len() <= 65536, "prompt exceeds bound");
    }
    Ok(text)
}

/// One connection has bounded framing, one reader and at most four active
/// workflow futures. Disconnect cancels them and waits for their cleanup.
pub async fn serve(
    service: EditorService,
    input: impl tokio::io::AsyncRead + Send + Unpin + 'static,
    output: impl tokio::io::AsyncWrite + Send + Unpin + 'static,
) -> Result<()> {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let reader = tokio::spawn(async move {
        let mut input = Wire::new(input, tokio::io::sink(), 64 * 1024 * 1024);
        loop {
            let message = input.read().await;
            let stop = message.is_err();
            if sender.send(message).await.is_err() || stop {
                break;
            }
        }
    });
    let mut output = Wire::new(tokio::io::empty(), output, 64 * 1024 * 1024);
    let mut initialized = false;
    let mut requests = BTreeSet::new();
    let mut active = BTreeMap::<String, Value>::new();
    let mut jobs = tokio::task::JoinSet::new();
    let mut progress = tokio::time::interval(Duration::from_secs(10));
    let result = async {
        loop {
            tokio::select! {
                message = receiver.recv() => {
                    let Some(Ok(message)) = message else { break; };
                    let request_id = message.get("id").cloned();
                    let method = message["method"].as_str().context("ACP request method required")?;
                    let params = &message["params"];
                    if method == "session/cancel" {
                        if let Ok(session_id) = session_id(params) { let _ = service.cancel(session_id).await; }
                        if let Some(id) = request_id { output.response_ok(id, json!({})).await?; }
                        continue;
                    }
                    let request_id = request_id.context("ACP request id required")?;
                    ensure!(request_id.is_null() || request_id.as_i64().is_some() || request_id.as_u64().is_some() || request_id.as_str().is_some_and(|id| !id.is_empty() && id.len() <= 128), "invalid ACP request id");
                    ensure!(requests.len() < 4096, "ACP request budget exhausted");
                    if !requests.insert(serde_json::to_string(&request_id)?) { output.response_error(request_id, -32600, "DUPLICATE_REQUEST_ID").await?; continue; }
                    let response = async {
                        if method == "initialize" {
                            ensure!(!initialized && params["protocolVersion"].as_u64().is_some_and(|version| version >= 1), "unsupported initialize");
                            initialized = true;
                            return Ok(Some(json!({"protocolVersion":1,"agentInfo":{"name":"orbit","version":env!("CARGO_PKG_VERSION")},"agentCapabilities":{"loadSession":true,"promptCapabilities":{}},"authMethods":[],"_meta":{"orbit":{"workflowAuthority":true,"managedCandidates":true}}})));
                        }
                        ensure!(initialized, "initialize first");
                        if method == "session/new" {
                            ensure!(params.get("mcpServers").is_none_or(|servers| servers == &json!([])), "external MCP authority is not admitted");
                            let session = service.new_session(Path::new(params["cwd"].as_str().context("cwd required")?)).await?;
                            let mode = if let Some(skill) = service.config().skill { let mode = serde_json::to_value(skill)?.as_str().unwrap().to_owned(); service.set_mode(&session.id, &mode).await?; mode } else {"auto".into()};
                            update(&service, &mut output, &session.id, commands()).await?;
                            return Ok(Some(json!({"sessionId":session.id,"modes":modes(&service,&mode)})));
                        }
                        let session_id = session_id(params)?.to_owned();
                        let session = service.session(&session_id).await?;
                        if service.config().external_role.is_some() {
                            ensure!(matches!(method, "session/load" | "session/prompt" | "_orbit/session/status" | "_orbit/session/diff" | "_orbit/reasoning/submit" | "_orbit/reasoning/freeze" | "_orbit/reasoning/accept"), "EXTERNAL_ROLE_AUTHORITY_DENIED");
                        }
                        match method {
                            "_orbit/reasoning/submit" => { let artifact = serde_json::from_value(params["artifact"].clone())?; let revision = service.submit_reasoning(&session_id, params["expectedRevision"].as_i64().context("expected revision required")?,params["requestId"].as_str().context("artifact request id required")?,&artifact).await?; Ok(Some(json!({"revision":revision}))) }
                            "_orbit/reasoning/freeze" => Ok(Some(json!({"workflowRunId":service.freeze_reasoning(&session_id,params["expectedRevision"].as_i64().context("expected revision required")?).await?}))),
                            "_orbit/reasoning/accept" => { service.accept_business(&session_id,&serde_json::from_value(params["acceptance"].clone())?).await?; Ok(Some(json!({}))) }
                            "session/load" => {
                                ensure!(params.get("mcpServers").is_none_or(|servers| servers == &json!([])), "external MCP authority is not admitted");
                                ensure!(Path::new(params["cwd"].as_str().context("cwd required")?).canonicalize()? == service.config().repository, "session repository mismatch");
                                for notification in service.notifications(&session_id).await? { output.notify("session/update", notification).await?; }
                                Ok(Some(json!({"modes":modes(&service,&session.mode)})))
                            }
                            "session/set_mode" => { let mode = params["modeId"].as_str().context("modeId required")?; service.set_mode(&session_id, mode).await?; update(&service, &mut output, &session_id, json!({"sessionUpdate":"current_mode_update","currentModeId":mode})).await?; Ok(Some(json!({}))) }
                            "_orbit/session/status" => Ok(Some(service.dashboard(&session_id).await?)),
                            "_orbit/session/open" => Ok(Some(json!({"workspacePath":session.worktree.context("worktree missing")?.workspace}))),
                            "_orbit/session/diff" => { let diff = session.worktree.context("worktree missing")?.diff().await?; let offset = params["offset"].as_u64().unwrap_or(0) as usize; ensure!(offset <= diff.len() && diff.is_char_boundary(offset), "invalid diff offset"); let mut end = (offset + 32 * 1024).min(diff.len()); while !diff.is_char_boundary(end) { end -= 1; } Ok(Some(json!({"diff":&diff[offset..end],"totalBytes":diff.len(),"nextOffset":if end < diff.len() {Some(end)} else {None},"truncated":end < diff.len()}))) }
                            "_orbit/candidate/recover_application" => { service.recover_application(&session_id, params["workspaceStateId"].as_str().context("candidate identity required")?).await?; Ok(Some(json!({}))) }
                            "_orbit/candidate/apply" | "_orbit/candidate/discard" => { service.candidate_action(&session_id, params["workspaceStateId"].as_str().context("candidate identity required")?, method.ends_with("apply")).await?; Ok(Some(json!({}))) }
                            "session/prompt" => {
                                let prompt = prompt_text(params)?;
                                if service.config().external_role.is_some() { ensure!(!prompt.starts_with('/') || matches!(prompt.trim(), "/status" | "/diff"), "EXTERNAL_ROLE_AUTHORITY_DENIED"); }
                                if service.config().external_role == Some(crate::workflow::reasoning::ExternalRole::BusinessAnalyst) { ensure!(prompt.starts_with('/'), "BA submits typed artifacts through the external-role interface"); }
                                text(&service, &mut output, &session_id, &prompt, true).await?;
                                if prompt.starts_with('/') {
                                    let mut parts = prompt.split_whitespace();
                                    match parts.next().unwrap_or("") {
                                        "/status" => { dashboard_update(&service,&mut output,&session_id).await?; return Ok(Some(json!({"stopReason":"end_turn"}))); }
                                        "/open" => { let path = session.worktree.context("worktree missing")?.workspace; text(&service,&mut output,&session_id,&format!("Managed attempt: `{}`",path.display()),false).await?; return Ok(Some(json!({"stopReason":"end_turn"}))); }
                                        "/diff" => { let diff = session.worktree.context("worktree missing")?.diff().await?; text(&service,&mut output,&session_id,&format!("```diff\n{diff}\n```"),false).await?; return Ok(Some(json!({"stopReason":"end_turn"}))); }
                                        "/cancel" => { service.cancel(&session_id).await?; dashboard_update(&service,&mut output,&session_id).await?; return Ok(Some(json!({"stopReason":"cancelled"}))); }
                                        command @ ("/apply" | "/discard") => { let expected = parts.next().context("candidate identity required")?; ensure!(parts.next().is_none(), "unexpected candidate action arguments"); service.candidate_action(&session_id,expected,command == "/apply").await?; dashboard_update(&service,&mut output,&session_id).await?; return Ok(Some(json!({"stopReason":"end_turn"}))); }
                                        "/continue" | "/review" => {}
                                        _ => anyhow::bail!("unknown Orbit command"),
                                    }
                                } else { ensure!(session.workflow_run_id.is_none(), "task is already pinned; use /continue or create a new session"); ensure!(!active.contains_key(&session_id) && active.len() < 4, "interactive execution capacity reached"); service.start(&session_id,&prompt).await?; }
                                ensure!(!active.contains_key(&session_id), "prompt already active for this session");
                                ensure!(active.len() < 4, "interactive execution capacity reached");
                                let review = prompt.trim() == "/review";
                                let worker = service.clone();
                                let worker_session = session_id.clone();
                                jobs.spawn(async move { let result = worker.run(&worker_session,review).await; (worker_session,result) });
                                active.insert(session_id,request_id.clone());
                                Ok(None)
                            }
                            _ => anyhow::bail!("unsupported Orbit ACP method"),
                        }
                    }.await;
                    match response { Ok(Some(value)) => output.response_ok(request_id,value).await?, Ok(None) => {}, Err(_) => output.response_error(request_id,-32603,"EDITOR_REQUEST_FAILED").await? }
                }
                completed = jobs.join_next(), if !jobs.is_empty() => {
                    let (session_id, result) = completed.context("editor worker disappeared")?.context("editor worker panicked")?;
                    let request_id = active.remove(&session_id).context("editor prompt ownership lost")?;
                    dashboard_update(&service,&mut output,&session_id).await?;
                    if result.is_err() { text(&service,&mut output,&session_id,"Orbit could not advance this workflow. Inspect its durable role, verification and cleanup evidence before resuming.",false).await?; }
                    let dashboard = service.dashboard(&session_id).await?;
                    if let Some(handoffs) = dashboard["handoffs"].as_array() {
                        for handoff in handoffs {
                            text(&service, &mut output, &session_id, &format!("\n{} handoff:\n```json\n{}\n```", handoff["role"].as_str().unwrap_or("role"), serde_json::to_string_pretty(&handoff["handoff"]["structured_payload"])?), false).await?;
                        }
                    }
                    output.response_ok(request_id,json!({"stopReason":if dashboard["workflow"]["status"] == "cancelled" {"cancelled"} else {"end_turn"}})).await?;
                }
                _ = progress.tick(), if !active.is_empty() => { for session_id in active.keys() { dashboard_update(&service,&mut output,session_id).await?; } }
            }
        }
        Ok::<_,anyhow::Error>(())
    }.await;
    for session_id in active.keys() {
        let _ = service.cancel(session_id).await;
    }
    while jobs.join_next().await.is_some() {}
    reader.abort();
    result
}
