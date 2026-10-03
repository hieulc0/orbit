//! ACP v1 stdio presentation and editor actions. Workflow decisions remain in
//! the coordinator; the wire exposes no implementer filesystem callbacks.
use super::editor_view;
use crate::workflow::flow::Skill;
use crate::{acp::wire::Wire, interactive::InteractiveService};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};

fn modes(service: &InteractiveService, current: &str) -> Value {
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
        (
            "preferences",
            "Inspect or set interaction/provider/model/reasoning/profile/flow",
        ),
        (
            "agents",
            "Inspect orchestrator and workflow agent selections",
        ),
        (
            "inspect",
            "Detailed workflow, verification, quota and audit inspector",
        ),
        ("cli", "Continue the same durable session in Orbit CLI"),
        (
            "decision",
            "Inspect durable Skill/Flow proposals and policy",
        ),
        (
            "start",
            "Accept a proposal in explicit Flow mode; pass its decision ID",
        ),
        (
            "close",
            "Close product after all flow candidates are discarded",
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
    service: &InteractiveService,
    output: &mut Wire,
    session_id: &str,
    update: Value,
) -> Result<()> {
    let params = json!({"sessionId":session_id,"update":update});
    // Native plans are reconstructed from durable domain state on load. They
    // are presentation snapshots, not part of the human conversation.
    if params["update"]["sessionUpdate"] != "plan" {
        service.record_notification(session_id, &params).await?;
    }
    output.notify("session/update", params).await
}

async fn text(
    service: &InteractiveService,
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
    editor_view::render_compact(dashboard)
}

fn render_inspector(dashboard: &Value) -> String {
    let workflow = &dashboard["workflow"];
    let session = dashboard
        .get("candidate_session")
        .unwrap_or(&dashboard["session"]);
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
    let counter = |group: &str, field: &str| {
        budget
            .and_then(|value| value[group][field].as_u64())
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unknown".into())
    };
    let calls = counter("usage", "total_calls");
    let max_calls = counter("limits", "max_total_calls");
    let reads = counter("usage", "file_read_bytes");
    let max_reads = counter("limits", "max_file_read_bytes");
    let flow = &dashboard["flow"];
    let mut panel = format!(
        "\n## Orbit task\n\n| State | Value |\n| --- | --- |\n| Task | `{}` |\n| Workflow | `{}` |\n| Flow / status | {} / {} |\n| Role | {} |\n| Provider / model / account | {} / {} / {} |\n| Repository calls | {} / {} |\n| File read bytes | {} / {} |\n| Candidate | `{}` |\n| Cleanup / execution | {} / {} |\n\n",
        workflow["task_id"].as_str().unwrap_or("not started"),
        workflow["id"].as_str().unwrap_or("not started"),
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
    panel.push_str(&format!(
        "Stage: {} · execution profile: {}\n",
        label(Some(workflow), "current_stage"),
        label(Some(&dashboard["execution_profile"]), "profile")
    ));
    for field in ["failure_reason", "cancellation_reason"] {
        if workflow[field].is_string() {
            panel.push_str(&format!("\nReason: {}\n", label(Some(workflow), field)));
        }
    }
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
    service: &InteractiveService,
    output: &mut Wire,
    session_id: &str,
) -> Result<()> {
    let dashboard = service.dashboard(session_id).await?;
    publish_dashboard(service, output, session_id, &dashboard).await
}

async fn publish_dashboard(
    service: &InteractiveService,
    output: &mut Wire,
    session_id: &str,
    dashboard: &Value,
) -> Result<()> {
    update(
        service,
        output,
        session_id,
        json!({"sessionUpdate":"plan","entries":editor_view::stage_entries(dashboard)}),
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
/// workflow futures. Disconnect drains admitted work to its normal gate;
/// only an explicit cancellation changes durable workflow authority.
pub async fn serve(
    service: InteractiveService,
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
    let mut progress = tokio::time::interval(Duration::from_secs(1));
    let mut last_snapshots = BTreeMap::<String, String>::new();
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
                            return Ok(Some(json!({"sessionId":session.id,"modes":modes(&service,&mode),"configOptions":session_options(&service,&session.id).await?})));
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
                                for notification in service.notifications(&session_id).await? {
                                    // Legacy plans are transient views too; preserve all
                                    // historical conversation text without text matching.
                                    if notification["update"]["sessionUpdate"] != "plan" {
                                        output.notify("session/update", notification).await?;
                                    }
                                }
                                dashboard_update(&service,&mut output,&session_id).await?;
                                Ok(Some(json!({"modes":modes(&service,&session.mode),"configOptions":session_options(&service,&session_id).await?})))
                            }
                            "session/set_config_option" => { service.set_preference(&session_id, params["configId"].as_str().context("configId required")?,params["value"].as_str().context("select value required")?).await?; let options=session_options(&service,&session_id).await?; update(&service,&mut output,&session_id,json!({"sessionUpdate":"config_option_update","configOptions":options})).await?; Ok(Some(json!({"configOptions":options}))) }
                            "session/set_mode" => { let mode = params["modeId"].as_str().context("modeId required")?; service.set_mode(&session_id, mode).await?; update(&service, &mut output, &session_id, json!({"sessionUpdate":"current_mode_update","currentModeId":mode})).await?; Ok(Some(json!({}))) }
                            "_orbit/session/status" => Ok(Some(service.dashboard(&session_id).await?)),
                            "_orbit/session/open" => {let target=service.flow_session(&session_id).await?.unwrap_or_else(||session_id.clone());Ok(Some(json!({"workspacePath":service.session(&target).await?.worktree.context("worktree missing")?.workspace})))}
                            "_orbit/session/diff" => {
                                let offset = params.get("offset").map(|value| value.as_u64().context("invalid diff offset")).transpose()?.unwrap_or(0);
                                Ok(Some(service.candidate_diff(&session_id, usize::try_from(offset)?).await?))
                            }
                            "_orbit/candidate/recover_application" => { service.recover_application(&session_id, params["workspaceStateId"].as_str().context("candidate identity required")?).await?; Ok(Some(json!({}))) }
                            "_orbit/candidate/apply" | "_orbit/candidate/discard" => { service.candidate_action(&session_id, params["workspaceStateId"].as_str().context("candidate identity required")?, method.ends_with("apply")).await?; Ok(Some(json!({}))) }
                            "session/prompt" => {
                                // Clients may register a new session only after
                                // handling its response. Refresh the command
                                // view before the first ordinary turn as well.
                                update(&service, &mut output, &session_id, commands()).await?;
                                let prompt = prompt_text(params)?;
                                if service.config().external_role.is_some() { ensure!(!prompt.starts_with('/') || matches!(prompt.trim(), "/status" | "/diff"), "EXTERNAL_ROLE_AUTHORITY_DENIED"); }
                                if service.config().external_role == Some(crate::workflow::reasoning::ExternalRole::BusinessAnalyst) { ensure!(prompt.starts_with('/'), "BA submits typed artifacts through the external-role interface"); }
                                text(&service, &mut output, &session_id, &prompt, true).await?;
                                if prompt.starts_with('/') {
                                    let mut parts = prompt.split_whitespace();
                                    match parts.next().unwrap_or("") {
                                        "/preferences" => {
                                            if let Some(key)=parts.next() { let value=parts.next().context("preference value required")?; ensure!(parts.next().is_none(), "unexpected preference arguments"); service.set_preference(&session_id,key,value).await?; }
                                            let options=session_options(&service,&session_id).await?;
                                            update(&service,&mut output,&session_id,json!({"sessionUpdate":"config_option_update","configOptions":options})).await?;
                                            text(&service,&mut output,&session_id,&format!("Preferences (not authority):\n```json\n{}\n```\nUse /preferences key value. Chat/Agent are read-only; Flow starts an explicit task. Gemini supports Auto reasoning only. Non-Auto reasoning uses the pinned Codex bridge with exact effort confirmation. Workflow agents remain independent.",serde_json::to_string_pretty(&service.preferences(&session_id).await?)?),false).await?;
                                            return Ok(Some(json!({"stopReason":"end_turn"})));
                                        }
                                        "/agents" | "/inspect" => { let dashboard=service.dashboard(&session_id).await?; let view=if prompt.trim()=="/agents" {editor_view::render_agents(&dashboard)} else {format!("{}\n{}",editor_view::render_agents(&dashboard),render_inspector(&dashboard))}; text(&service,&mut output,&session_id,&view,false).await?; return Ok(Some(json!({"stopReason":"end_turn"}))); }
                                        "/decision" => {text(&service,&mut output,&session_id,&editor_view::render_decisions(&service.dashboard(&session_id).await?,true),false).await?;return Ok(Some(json!({"stopReason":"end_turn"})));}
                                        "/start" => {let decision=parts.next().context("decision identity required")?;ensure!(parts.next().is_none(),"unexpected start arguments");let workflow=service.accept_decision(&session_id,decision).await?;dashboard_update(&service,&mut output,&session_id).await?;text(&service,&mut output,&session_id,&format!("Proposal accepted as `{workflow}`. Use /continue to run the current flow. Replaying acceptance creates no new work."),false).await?;return Ok(Some(json!({"stopReason":"end_turn"})));}
                                        "/close" => {service.close_product(&session_id).await?;text(&service,&mut output,&session_id,"Product session closed; managed context cleaned.",false).await?;return Ok(Some(json!({"stopReason":"end_turn"})));}
                                        "/cli" => { text(&service,&mut output,&session_id,&service.cli_handoff(&session_id),false).await?; return Ok(Some(json!({"stopReason":"end_turn"}))); }
                                        "/status" => { let dashboard=service.dashboard(&session_id).await?;publish_dashboard(&service,&mut output,&session_id,&dashboard).await?;text(&service,&mut output,&session_id,&render_dashboard(&dashboard),false).await?; return Ok(Some(json!({"stopReason":"end_turn"}))); }
                                        "/open" => { let target=service.flow_session(&session_id).await?.unwrap_or_else(||session_id.clone());let path = service.session(&target).await?.worktree.context("worktree missing")?.workspace; text(&service,&mut output,&session_id,&format!("Managed attempt: `{}`",path.display()),false).await?; return Ok(Some(json!({"stopReason":"end_turn"}))); }
                                        "/diff" => {
                                            let offset = parts.next().map(str::parse::<usize>).transpose()?.unwrap_or(0);
                                            ensure!(parts.next().is_none(), "unexpected diff arguments");
                                            let page = service.candidate_diff(&session_id, offset).await?;
                                            text(&service,&mut output,&session_id,&format!("```diff\n{}\n```\nNext offset: {}",page["diff"].as_str().unwrap_or(""),page["nextOffset"]),false).await?;
                                            return Ok(Some(json!({"stopReason":"end_turn"})));
                                        }
                                        "/cancel" => { service.cancel(&session_id).await?; dashboard_update(&service,&mut output,&session_id).await?;text(&service,&mut output,&session_id,"Cancellation requested. Use /status to inspect durable execution and cleanup state.",false).await?; return Ok(Some(json!({"stopReason":"cancelled"}))); }
                                        command @ ("/apply" | "/discard") => { let expected = parts.next().context("candidate identity required")?; ensure!(parts.next().is_none(), "unexpected candidate action arguments"); service.candidate_action(&session_id,expected,command == "/apply").await?; dashboard_update(&service,&mut output,&session_id).await?;text(&service,&mut output,&session_id,if command == "/apply" {"Exact candidate applied."} else {"Candidate discarded."},false).await?; return Ok(Some(json!({"stopReason":"end_turn"}))); }
                                        "/continue" | "/review" => {}
                                        _ => anyhow::bail!("unknown Orbit command"),
                                    }
                                } else {
                                    ensure!(!active.contains_key(&session_id) && active.len() < 4, "interactive execution capacity reached");
                                    service.start_conversation(&session_id,&prompt).await?;
                                }
                                ensure!(!active.contains_key(&session_id), "prompt already active for this session");
                                ensure!(active.len() < 4, "interactive execution capacity reached");
                                let review = prompt.trim() == "/review";
                                let worker = service.clone();
                                let worker_session = session_id.clone();
                                let conversation = service.session(&session_id).await?.state == "STARTING";
                                jobs.spawn(async move { let result = if conversation { worker.run_conversation(&worker_session).await } else { worker.run(&worker_session,review).await }; (worker_session,result,conversation) });
                                active.insert(session_id,request_id.clone());
                                Ok(None)
                            }
                            _ => anyhow::bail!("unsupported Orbit ACP method"),
                        }
                    }.await;
                    match response {
                        Ok(Some(value)) => {
                            let command_session = match method {
                                "session/new" => value["sessionId"].as_str().map(str::to_owned),
                                "session/load" => params["sessionId"].as_str().map(str::to_owned),
                                _ => None,
                            };
                            output.response_ok(request_id,value).await?;
                            if let Some(session_id) = command_session {
                                update(&service, &mut output, &session_id, commands()).await?;
                            }
                        }
                        Ok(None) => {},
                        Err(error) => {
                            let reason=error.to_string();
                            let code=reason.split([':', ' ']).next().unwrap_or("EDITOR_REQUEST_FAILED");
                            let message=if code.len()<=64 && code.bytes().all(|b|b.is_ascii_uppercase()||b==b'_') {code} else {"EDITOR_REQUEST_FAILED"};
                            output.response_error(request_id,-32603,message).await?;
                        }
                    }
                }
                completed = jobs.join_next(), if !jobs.is_empty() => {
                    let (session_id, result, conversation) = completed.context("editor worker disappeared")?.context("editor worker panicked")?;
                    let request_id = active.remove(&session_id).context("editor prompt ownership lost")?;
                    last_snapshots.remove(&session_id);
                    dashboard_update(&service,&mut output,&session_id).await?;
                    if result.is_err() { text(&service,&mut output,&session_id,"Orbit could not advance this workflow. Inspect its durable role, verification and cleanup evidence before resuming.",false).await?; }
                    let dashboard = service.dashboard(&session_id).await?;
                    if conversation {
                        text(&service,&mut output,&session_id,&editor_view::render_conversation(&dashboard),false).await?;
                    }
                    output.response_ok(request_id,json!({"stopReason":if (conversation && dashboard["decisions"].as_array().and_then(|v|v.last()).is_some_and(|v|v["status"]=="CANCELLED")) || (conversation && dashboard["orchestrator"].as_array().and_then(|v|v.last()).is_some_and(|v|v["status"]=="CANCELLED")) || (!conversation && dashboard["workflow"]["status"] == "cancelled") {"cancelled"} else {"end_turn"}})).await?;
                }
                _ = progress.tick(), if !active.is_empty() => {
                    for session_id in active.keys() {
                        let dashboard = service.dashboard(session_id).await?;
                        let digest = crate::model::digest(&serde_json::to_vec(&editor_view::stage_entries(&dashboard))?);
                        if last_snapshots.get(session_id) != Some(&digest) {
                            publish_dashboard(&service,&mut output,session_id,&dashboard).await?;
                            last_snapshots.insert(session_id.clone(),digest);
                        }
                    }
                }
            }
        }
        Ok::<_,anyhow::Error>(())
    }.await;
    // A connection owns delivery, not the workflow. Retain execution futures
    // until their coordinator gate and supervised cleanup even if output fails.
    // Another client may reconnect, inspect, or explicitly cancel that work.
    while jobs.join_next().await.is_some() {}
    reader.abort();
    result
}

async fn session_options(service: &InteractiveService, session: &str) -> Result<Value> {
    Ok(editor_view::config_options(
        &serde_json::to_value(service.preferences(session).await?)?,
        matches!(
            service.config().agent_execution_profile,
            crate::execution::local::RoleExecutionProfile::DevLocal { .. }
        ),
        service.config().skill.is_some(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        execution::local::RoleExecutionProfile,
        interactive::ServiceConfig,
        regression_strategy::{SelectionPolicy, VerificationCheck, VerificationTier},
        workflow::flow::Risk,
        workflow_coordinator::{SimulatedRoleExecutor, WorkflowCoordinator},
    };
    use std::{os::unix::fs::PermissionsExt, sync::Arc};

    #[tokio::test]
    async fn native_plan_publication_is_transient_and_clears_absent_workflows() -> Result<()> {
        let repository = tempfile::tempdir()?;
        let workspaces = tempfile::tempdir()?;
        std::fs::set_permissions(workspaces.path(), std::fs::Permissions::from_mode(0o700))?;
        let mut policy = SelectionPolicy::new("presentation", "Presentation fixture");
        policy.canonical_digest = true;
        policy.checks.push(VerificationCheck::new_command(
            "fixture",
            "Fixture",
            vec![VerificationTier::Fast],
            vec!["true".into()],
        ));
        // No database is available: presentation must not persist snapshots.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://fixture:fixture@127.0.0.1:9/fixture")?;
        let service = InteractiveService::new(
            pool.clone(),
            ServiceConfig {
                repository: repository.path().canonicalize()?,
                workspaces: workspaces.path().canonicalize()?,
                agent_execution_profile: RoleExecutionProfile::Trusted,
                verification_environment: serde_json::from_value(json!({
                    "execution_profile":"sandboxed-container","isolation":"rootless-podman",
                    "oci_runtime":"podman","runtime_image":"localhost/fixture",
                    "runtime_image_digest":format!("sha256:{}", "a".repeat(64)),
                    "architecture":"x86_64","os":"linux","orbit_version":"fixture"
                }))?,
                selection_policy: policy,
                risk: Risk::Conservative,
                skill: None,
                external_role: None,
            },
            Arc::new(WorkflowCoordinator::new(
                pool,
                Arc::new(SimulatedRoleExecutor::new()),
            )),
        )?;
        let (read, write) = tokio::io::duplex(65536);
        let mut output = Wire::new(tokio::io::empty(), write, 65536);
        let mut input = Wire::new(read, tokio::io::sink(), 65536);
        let workflow = json!({"workflow":{"status":"planning"},
            "flow":{"review_tier":"STANDARD","completion_tier":"FULL"}});
        for dashboard in [
            workflow,
            json!({"orchestrator":[{"status":"PLANNING"}]}),
            json!({"orchestrator":[{"status":"COMPLETED"}],"changed_files":{"total":0}}),
        ] {
            publish_dashboard(&service, &mut output, "presentation", &dashboard).await?;
            let notification = input.read().await?;
            assert_eq!(notification["params"]["update"]["sessionUpdate"], "plan");
            assert_eq!(
                notification["params"]["update"]["entries"],
                json!(editor_view::stage_entries(&dashboard))
            );
        }
        drop(output);
        assert!(
            input.read().await.is_err(),
            "dashboard emitted unsolicited conversation content"
        );
        Ok(())
    }
}
