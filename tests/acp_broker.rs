use anyhow::Result;
use orbit::{
    acp_broker::Broker,
    acp_contract::Limits,
    acp_runtime::{WORKSPACE, acp_request, select_model},
    acp_wire::Wire,
    coding_agent::Session,
    execution::{Backend, Profile},
    model::*,
    repository::Workspace,
    worker::Client,
};
use serde_json::json;
use std::collections::BTreeMap;

async fn start_mock_server() -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut rx => break,
                Ok((mut socket, _)) = listener.accept() => {
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = [0u8; 4096];
                        let _ = socket.read(&mut buf).await;
                        let body = r#"{"status":"accepted","replayed":false}"#;
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = socket.write_all(resp.as_bytes()).await;
                    });
                }
            }
        }
    });
    (format!("http://127.0.0.1:{}", addr.port()), tx)
}

fn create_assignment(acp_limits: Limits) -> Result<Assignment> {
    let mut def = Definition::parse(include_str!("../examples/agent.yaml"))?;
    def.steps.retain(|name, _| name == "planner");
    let step = def.steps.get_mut("planner").unwrap();
    let mut agent = step.agent.clone().unwrap();
    agent.acp_limits = Some(acp_limits);
    step.agent = Some(agent);

    let plan = Plan {
        definition: def,
        digest: "a".repeat(64),
        repository: RepositoryBinding::none(),
        agent_bindings: BTreeMap::new(),
        execution_profiles: BTreeMap::new(),
        scope: None,
    };

    Ok(Assignment {
        run_id: "test-run".into(),
        task_id: "test-task".into(),
        attempt_id: "test-attempt".into(),
        generation: 1,
        workspace_id: "test-ws".into(),
        lease_token: "test-lease".into(),
        lease_expires_at: 9999999999,
        heartbeat_interval: 30,
        deadline_at: 9999999999,
        plan,
        step: "planner".into(),
        input_artifacts: vec![],
        idempotency_key: "test-key".into(),
        gpu_devices: vec![],
        agent_binding_digest: Some("test-digest".into()),
        execution_id: None,
    })
}

#[tokio::test]
async fn test_broker_recoverable_tool_error_preserves_session() -> Result<()> {
    let (server_url, _shutdown) = start_mock_server().await;
    let client = Client::new(server_url, "test-token".into())?;

    let root = tempfile::tempdir()?;
    let ws_path = root.path().join("workspace");
    std::fs::create_dir_all(&ws_path)?;
    std::fs::write(ws_path.join("existing.txt"), "hello world\nline 2\n")?;

    let limits = Limits {
        prompt_turns: 1,
        broker_calls: 10,
        reported_tool_calls: 10,
        turn_timeout_seconds: 10,
        terminal_timeout_seconds: 5,
        terminal_runtime_seconds: 10,
        output_bytes: 65536,
    };
    let assignment = create_assignment(limits)?;
    let workspace = Workspace {
        path: ws_path.clone(),
        git_dir: root.path().join("git"),
        home: root.path().join("home"),
    };
    let profile = Profile {
        backend: Backend::RootlessPodman,
        image: "test-image".into(),
    };
    let creds = BTreeMap::new();

    let session = Session {
        client: &client,
        assignment: &assignment,
        workspace: &workspace,
        directory: root.path(),
        home: root.path(),
        profile: &profile,
        credentials: &creds,
    };

    let mut broker = Broker::new(session);
    broker.session_id = Some("test-session-1".into());
    broker.session_digest = "test-session-digest".into();
    broker.active = true;

    let (pipe_in_read, pipe_in_write) = tokio::io::duplex(4096);
    let (pipe_out_read, pipe_out_write) = tokio::io::duplex(4096);
    let mut wire = Wire::new(pipe_in_read, pipe_out_write, 1024 * 1024);
    let mut peer_wire = Wire::new(pipe_out_read, pipe_in_write, 1024 * 1024);

    // Call 1a: Invalid path outside workspace (recoverable -32602)
    let bad_path_req = json!({
        "jsonrpc": "2.0",
        "id": "req-1a",
        "method": "fs/read_text_file",
        "params": {
            "sessionId": "test-session-1",
            "path": "/etc/passwd"
        }
    });

    broker.message(&mut wire, bad_path_req).await?;
    let resp1a = peer_wire.read().await?;
    assert_eq!(resp1a["id"], "req-1a");
    assert_eq!(resp1a["error"]["code"], -32602);
    assert!(!broker.poisoned);
    assert_eq!(broker.tool_failures, 1);

    // Call 1b: Non-existent file inside workspace (recoverable tool error -32603)
    let bad_read_req = json!({
        "jsonrpc": "2.0",
        "id": "req-1b",
        "method": "fs/read_text_file",
        "params": {
            "sessionId": "test-session-1",
            "path": format!("{WORKSPACE}/nonexistent.txt")
        }
    });

    broker.message(&mut wire, bad_read_req).await?;

    // Verify wire received a JSON-RPC error response with code -32603
    let resp1b = peer_wire.read().await?;
    assert_eq!(resp1b["id"], "req-1b");
    assert!(resp1b.get("error").is_some());
    assert_eq!(resp1b["error"]["code"], -32603);

    // Invariant: Session is NOT poisoned, failures are tracked
    assert!(
        !broker.poisoned,
        "broker must NOT be poisoned on tool error"
    );
    assert_eq!(broker.tool_calls, 2);
    assert_eq!(broker.tool_failures, 2);
    assert_eq!(broker.tool_successes, 0);

    // Call 2: Read existing file on the SAME session
    let good_read_req = json!({
        "jsonrpc": "2.0",
        "id": "req-2",
        "method": "fs/read_text_file",
        "params": {
            "sessionId": "test-session-1",
            "path": format!("{WORKSPACE}/existing.txt"),
            "line": 1,
            "limit": 10
        }
    });

    broker.message(&mut wire, good_read_req).await?;

    // Verify wire received a JSON-RPC success response!
    let resp2 = peer_wire.read().await?;
    assert_eq!(resp2["id"], "req-2");
    assert!(resp2.get("result").is_some());
    assert_eq!(resp2["result"]["content"], "hello world\nline 2\n");

    // Invariants: Session remains healthy, success is recorded
    assert!(!broker.poisoned, "broker must remain unpoisoned");
    assert_eq!(broker.tool_calls, 3);
    assert_eq!(broker.tool_failures, 2);
    assert_eq!(broker.tool_successes, 1);
    assert_eq!(broker.tool_counts.get("read_file"), Some(&3));

    // Call 3: Fatal error - foreign session ID
    let fatal_req = json!({
        "jsonrpc": "2.0",
        "id": "req-3",
        "method": "fs/read_text_file",
        "params": {
            "sessionId": "wrong-session-id",
            "path": format!("{WORKSPACE}/existing.txt")
        }
    });

    let res = broker.message(&mut wire, fatal_req).await;
    assert!(res.is_err(), "fatal error must bail");
    assert!(broker.poisoned, "broker MUST be poisoned on fatal error");

    Ok(())
}

#[tokio::test]
async fn test_wire_error_responses_do_not_poison_framing() -> Result<()> {
    let (pipe_in_read, pipe_in_write) = tokio::io::duplex(4096);
    let (pipe_out_read, pipe_out_write) = tokio::io::duplex(4096);
    let mut server_wire = Wire::new(pipe_in_read, pipe_out_write, 1024 * 1024);
    let mut client_wire = Wire::new(pipe_out_read, pipe_in_write, 1024 * 1024);

    // Send error response
    server_wire
        .response_error(json!("req-1"), -32603, "temporary tool failure")
        .await?;

    let msg = client_wire.read().await?;
    assert_eq!(msg["id"], "req-1");
    assert_eq!(msg["error"]["code"], -32603);
    assert_eq!(msg["error"]["message"], "temporary tool failure");

    // Subsequent normal response on same wire
    server_wire
        .response_ok(json!("req-2"), json!({"content": "recovered"}))
        .await?;

    let msg2 = client_wire.read().await?;
    assert_eq!(msg2["id"], "req-2");
    assert_eq!(msg2["result"]["content"], "recovered");

    Ok(())
}

#[tokio::test]
async fn test_model_selection_lifecycle_scenarios() -> Result<()> {
    let (server_url, _shutdown) = start_mock_server().await;
    let client = Client::new(server_url, "test-token".into())?;

    let root = tempfile::tempdir()?;
    let ws_path = root.path().join("workspace");
    std::fs::create_dir_all(&ws_path)?;

    let limits = Limits {
        prompt_turns: 1,
        broker_calls: 10,
        reported_tool_calls: 10,
        turn_timeout_seconds: 10,
        terminal_timeout_seconds: 5,
        terminal_runtime_seconds: 10,
        output_bytes: 65536,
    };
    let assignment = create_assignment(limits)?;
    let workspace = Workspace {
        path: ws_path,
        git_dir: root.path().join("git"),
        home: root.path().join("home"),
    };
    let profile = Profile {
        backend: Backend::RootlessPodman,
        image: "test-image".into(),
    };
    let creds = BTreeMap::new();

    let make_session = || Session {
        client: &client,
        assignment: &assignment,
        workspace: &workspace,
        directory: root.path(),
        home: root.path(),
        profile: &profile,
        credentials: &creds,
    };

    // Scenario A: Requested model matches initial model -> no wire activity
    {
        let mut broker = Broker::new(make_session());
        let (pipe_in_read, _pipe_in_write) = tokio::io::duplex(4096);
        let (_pipe_out_read, pipe_out_write) = tokio::io::duplex(4096);
        let mut wire = Wire::new(pipe_in_read, pipe_out_write, 1024 * 1024);

        select_model(
            &mut wire,
            &mut broker,
            "sess-1",
            Some("gemini-3.8-flash"),
            Some("gemini-3.8-flash"),
        )
        .await?;
    }

    // Scenario B: Requested model activates via session/set_config_option
    {
        let mut broker = Broker::new(make_session());
        let (pipe_in_read, pipe_in_write) = tokio::io::duplex(4096);
        let (pipe_out_read, pipe_out_write) = tokio::io::duplex(4096);
        let mut wire = Wire::new(pipe_in_read, pipe_out_write, 1024 * 1024);
        let mut peer_wire = Wire::new(pipe_out_read, pipe_in_write, 1024 * 1024);

        let select_fut = select_model(
            &mut wire,
            &mut broker,
            "sess-2",
            Some("gemini-3.8-flash"),
            Some("gemini-3.7-flash"),
        );

        let peer_fut = async {
            // Peer receives session/set_config_option
            let req = peer_wire.read().await?;
            assert_eq!(req["method"], "session/set_config_option");
            assert_eq!(req["params"]["configId"], "model");
            assert_eq!(req["params"]["value"], "gemini-3.8-flash");

            // Peer responds with confirmed configOptions
            peer_wire
                .response_ok(
                    req["id"].clone(),
                    json!({
                        "configOptions": [
                            {
                                "id": "model",
                                "currentValue": "gemini-3.8-flash"
                            }
                        ]
                    }),
                )
                .await?;
            Ok::<(), anyhow::Error>(())
        };

        let (select_res, peer_res) = tokio::join!(select_fut, peer_fut);
        assert!(select_res.is_ok());
        assert!(peer_res.is_ok());
    }

    // Scenario C: session/set_config_option fails, falls back to session/set_model
    {
        let mut broker = Broker::new(make_session());
        let (pipe_in_read, pipe_in_write) = tokio::io::duplex(4096);
        let (pipe_out_read, pipe_out_write) = tokio::io::duplex(4096);
        let mut wire = Wire::new(pipe_in_read, pipe_out_write, 1024 * 1024);
        let mut peer_wire = Wire::new(pipe_out_read, pipe_in_write, 1024 * 1024);

        let select_fut = select_model(
            &mut wire,
            &mut broker,
            "sess-3",
            Some("gemini-3.8-flash"),
            Some("gemini-3.7-flash"),
        );

        let peer_fut = async {
            // 1. Peer receives session/set_config_option and returns method not found (-32601)
            let req1 = peer_wire.read().await?;
            assert_eq!(req1["method"], "session/set_config_option");
            peer_wire
                .response_error(req1["id"].clone(), -32601, "Method not found")
                .await?;

            // 2. Peer receives fallback session/set_model
            let req2 = peer_wire.read().await?;
            assert_eq!(req2["method"], "session/set_model");
            assert_eq!(req2["params"]["modelId"], "gemini-3.8-flash");

            // Peer confirms model in currentModelId
            peer_wire
                .response_ok(
                    req2["id"].clone(),
                    json!({
                        "models": {
                            "currentModelId": "gemini-3.8-flash"
                        }
                    }),
                )
                .await?;
            Ok::<(), anyhow::Error>(())
        };

        let (select_res, peer_res) = tokio::join!(select_fut, peer_fut);
        assert!(select_res.is_ok());
        assert!(peer_res.is_ok());
    }

    // Scenario D: Server rejects model (e.g. invalid/unsupported model -32602) -> fails closed
    {
        let mut broker = Broker::new(make_session());
        let (pipe_in_read, pipe_in_write) = tokio::io::duplex(4096);
        let (pipe_out_read, pipe_out_write) = tokio::io::duplex(4096);
        let mut wire = Wire::new(pipe_in_read, pipe_out_write, 1024 * 1024);
        let mut peer_wire = Wire::new(pipe_out_read, pipe_in_write, 1024 * 1024);

        let select_fut = select_model(
            &mut wire,
            &mut broker,
            "sess-4",
            Some("unknown-model"),
            Some("gemini-3.7-flash"),
        );

        let peer_fut = async {
            // 1. Peer returns error on set_config_option
            let req1 = peer_wire.read().await?;
            peer_wire
                .response_error(req1["id"].clone(), -32602, "Invalid model")
                .await?;

            // 2. Peer returns error on fallback set_model
            let req2 = peer_wire.read().await?;
            peer_wire
                .response_error(req2["id"].clone(), -32602, "Invalid model")
                .await?;
            Ok::<(), anyhow::Error>(())
        };

        let (select_res, peer_res) = tokio::join!(select_fut, peer_fut);
        assert!(
            select_res.is_err(),
            "must fail closed when model is rejected"
        );
        assert!(peer_res.is_ok());
    }

    // Scenario E: Server confirms mismatched model -> fails closed
    {
        let mut broker = Broker::new(make_session());
        let (pipe_in_read, pipe_in_write) = tokio::io::duplex(4096);
        let (pipe_out_read, pipe_out_write) = tokio::io::duplex(4096);
        let mut wire = Wire::new(pipe_in_read, pipe_out_write, 1024 * 1024);
        let mut peer_wire = Wire::new(pipe_out_read, pipe_in_write, 1024 * 1024);

        let select_fut = select_model(
            &mut wire,
            &mut broker,
            "sess-5",
            Some("gemini-3.8-flash"),
            Some("gemini-3.7-flash"),
        );

        let peer_fut = async {
            // 1. Error on set_config_option
            let req1 = peer_wire.read().await?;
            peer_wire
                .response_error(req1["id"].clone(), -32601, "unsupported")
                .await?;

            // 2. set_model returns mismatched model
            let req2 = peer_wire.read().await?;
            peer_wire
                .response_ok(
                    req2["id"].clone(),
                    json!({
                        "models": {
                            "currentModelId": "gemini-3.7-flash"
                        }
                    }),
                )
                .await?;
            Ok::<(), anyhow::Error>(())
        };

        let (select_res, peer_res) = tokio::join!(select_fut, peer_fut);
        assert!(
            select_res.is_err(),
            "must fail closed when model confirmation mismatches"
        );
        assert!(peer_res.is_ok());
    }

    // Scenario F: Invalid model identifier (e.g. injection) -> fails before wire traffic
    {
        let mut broker = Broker::new(make_session());
        let (pipe_in_read, _pipe_in_write) = tokio::io::duplex(4096);
        let (_pipe_out_read, pipe_out_write) = tokio::io::duplex(4096);
        let mut wire = Wire::new(pipe_in_read, pipe_out_write, 1024 * 1024);

        let res = select_model(
            &mut wire,
            &mut broker,
            "sess-6",
            Some("model;rm -rf /"),
            Some("gemini-3.7-flash"),
        )
        .await;

        assert!(
            res.is_err(),
            "must reject invalid model names before sending commands"
        );
    }

    Ok(())
}

async fn start_budget_mock_server(
    allowed_reservations: usize,
) -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    tokio::spawn(async move {
        loop {
            let counter = counter.clone();
            tokio::select! {
                _ = &mut rx => break,
                Ok((mut socket, _)) = listener.accept() => {
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = [0u8; 8192];
                        let n = socket.read(&mut buf).await.unwrap_or(0);
                        let req_str = String::from_utf8_lossy(&buf[..n]);
                        let (status, body) = if req_str.contains("reserve_agent_call") {
                            let count = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            if count >= allowed_reservations {
                                ("400 Bad Request", r#"{"error":"agent budget exhausted","code":"budget_exhausted"}"#)
                            } else {
                                ("200 OK", r#"{"status":"accepted","replayed":false}"#)
                            }
                        } else {
                            ("200 OK", r#"{"status":"accepted","replayed":false}"#)
                        };
                        let resp = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = socket.write_all(resp.as_bytes()).await;
                    });
                }
            }
        }
    });
    (format!("http://127.0.0.1:{}", addr.port()), tx)
}

#[tokio::test]
async fn test_broker_budget_exhaustion_terminates_cleanly_without_poisoning() -> Result<()> {
    // Mock server allows exactly 1 reservation; 2nd reservation returns budget exhausted
    let (server_url, _shutdown) = start_budget_mock_server(1).await;
    let client = Client::new(server_url, "test-token".into())?;

    let root = tempfile::tempdir()?;
    let ws_path = root.path().join("workspace");
    std::fs::create_dir_all(&ws_path)?;
    std::fs::write(ws_path.join("file.txt"), "budget test file\n")?;

    let limits = Limits {
        prompt_turns: 1,
        broker_calls: 10,
        reported_tool_calls: 10,
        turn_timeout_seconds: 10,
        terminal_timeout_seconds: 5,
        terminal_runtime_seconds: 10,
        output_bytes: 65536,
    };
    let assignment = create_assignment(limits)?;
    let workspace = Workspace {
        path: ws_path.clone(),
        git_dir: root.path().join("git"),
        home: root.path().join("home"),
    };
    let profile = Profile {
        backend: Backend::RootlessPodman,
        image: "test-image".into(),
    };
    let creds = BTreeMap::new();

    let session = Session {
        client: &client,
        assignment: &assignment,
        workspace: &workspace,
        directory: root.path(),
        home: root.path(),
        profile: &profile,
        credentials: &creds,
    };

    let mut broker = Broker::new(session);
    broker.session_id = Some("test-session-budget".into());
    broker.session_digest = "test-session-digest".into();
    broker.active = true;

    let (pipe_in_read, pipe_in_write) = tokio::io::duplex(4096);
    let (pipe_out_read, pipe_out_write) = tokio::io::duplex(4096);
    let mut wire = Wire::new(pipe_in_read, pipe_out_write, 1024 * 1024);
    let mut peer_wire = Wire::new(pipe_out_read, pipe_in_write, 1024 * 1024);

    // Call 1: 1st reservation succeeds
    let req1 = json!({
        "jsonrpc": "2.0",
        "id": "req-1",
        "method": "fs/read_text_file",
        "params": {
            "sessionId": "test-session-budget",
            "path": format!("{WORKSPACE}/file.txt"),
            "line": 1,
            "limit": 10
        }
    });

    broker.message(&mut wire, req1).await?;
    let resp1 = peer_wire.read().await?;
    assert_eq!(resp1["id"], "req-1");
    assert_eq!(resp1["result"]["content"], "budget test file\n");
    assert!(!broker.poisoned);
    assert_eq!(broker.tool_calls, 1);
    assert_eq!(broker.tool_successes, 1);
    assert_eq!(broker.tool_failures, 0);

    // Call 2: 2nd reservation rejected with 400 budget exhausted
    let req2 = json!({
        "jsonrpc": "2.0",
        "id": "req-2",
        "method": "fs/read_text_file",
        "params": {
            "sessionId": "test-session-budget",
            "path": format!("{WORKSPACE}/file.txt"),
            "line": 1,
            "limit": 10
        }
    });

    let res = broker.message(&mut wire, req2).await;
    assert!(
        res.is_err(),
        "execution limit must return Err to abort message loop"
    );
    let err = res.unwrap_err();
    assert!(
        err.downcast_ref::<orbit::acp_broker::ExecutionLimitError>()
            .is_some(),
        "error must be ExecutionLimitError"
    );

    // Crucial invariants:
    // 1. Broker must NOT be poisoned
    assert!(
        !broker.poisoned,
        "broker must NOT be poisoned on budget exhaustion"
    );
    // 2. Execution limit state is preserved on broker
    assert_eq!(
        broker.execution_limit.as_ref().map(|(r, _)| *r),
        Some(orbit::continuation::TerminationReason::BudgetExhausted)
    );
    // 3. Tool failure count tracked
    assert_eq!(broker.tool_failures, 1);

    // 4. Wire received clean JSON-RPC -32000 error with execution_limit indication
    let err_resp = peer_wire.read().await?;
    assert_eq!(err_resp["id"], "req-2");
    assert_eq!(err_resp["error"]["code"], -32000);
    assert!(
        err_resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("budget exhausted")
    );

    // 5. Wire received session/cancel notification
    let cancel_notif = peer_wire.read().await?;
    assert_eq!(cancel_notif["method"], "session/cancel");
    assert_eq!(cancel_notif["params"]["sessionId"], "test-session-budget");

    Ok(())
}

#[tokio::test]
async fn test_broker_server_fatal_error_poisons_session() -> Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut rx => break,
                Ok((mut socket, _)) = listener.accept() => {
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = [0u8; 4096];
                        let _ = socket.read(&mut buf).await;
                        let body = r#"{"error":"internal server failure"}"#;
                        let resp = format!(
                            "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = socket.write_all(resp.as_bytes()).await;
                    });
                }
            }
        }
    });

    let client = Client::new(
        format!("http://127.0.0.1:{}", addr.port()),
        "test-token".into(),
    )?;
    let root = tempfile::tempdir()?;
    let ws_path = root.path().join("workspace");
    std::fs::create_dir_all(&ws_path)?;
    std::fs::write(ws_path.join("file.txt"), "hello\n")?;

    let limits = Limits {
        prompt_turns: 1,
        broker_calls: 10,
        reported_tool_calls: 10,
        turn_timeout_seconds: 10,
        terminal_timeout_seconds: 5,
        terminal_runtime_seconds: 10,
        output_bytes: 65536,
    };
    let assignment = create_assignment(limits)?;
    let workspace = Workspace {
        path: ws_path,
        git_dir: root.path().join("git"),
        home: root.path().join("home"),
    };
    let profile = Profile {
        backend: Backend::RootlessPodman,
        image: "test-image".into(),
    };
    let creds = BTreeMap::new();

    let session = Session {
        client: &client,
        assignment: &assignment,
        workspace: &workspace,
        directory: root.path(),
        home: root.path(),
        profile: &profile,
        credentials: &creds,
    };

    let mut broker = Broker::new(session);
    broker.session_id = Some("test-session-fatal".into());
    broker.session_digest = "test-session-digest".into();
    broker.active = true;

    let (pipe_in_read, pipe_in_write) = tokio::io::duplex(4096);
    let (pipe_out_read, pipe_out_write) = tokio::io::duplex(4096);
    let mut wire = Wire::new(pipe_in_read, pipe_out_write, 1024 * 1024);
    let mut peer_wire = Wire::new(pipe_out_read, pipe_in_write, 1024 * 1024);

    let req = json!({
        "jsonrpc": "2.0",
        "id": "req-fatal",
        "method": "fs/read_text_file",
        "params": {
            "sessionId": "test-session-fatal",
            "path": format!("{WORKSPACE}/file.txt"),
            "line": 1,
            "limit": 10
        }
    });

    let res = broker.message(&mut wire, req).await;
    assert!(res.is_err());
    // Invariant: Broker MUST be poisoned on fatal infrastructure failure
    assert!(
        broker.poisoned,
        "broker must be poisoned on fatal server error"
    );
    assert!(broker.execution_limit.is_none());

    let err_resp = peer_wire.read().await?;
    assert_eq!(err_resp["id"], "req-fatal");
    assert_eq!(err_resp["error"]["code"], -32603);

    let _ = tx.send(());
    Ok(())
}

#[tokio::test]
async fn test_wire_sync_interleaved_session_update_exact_q3_sequence() -> Result<()> {
    let (server_url, _shutdown) = start_mock_server().await;
    let client = Client::new(server_url, "test-token".into())?;

    let root = tempfile::tempdir()?;
    let ws_path = root.path().join("workspace");
    std::fs::create_dir_all(&ws_path)?;

    let limits = Limits {
        prompt_turns: 1,
        broker_calls: 10,
        reported_tool_calls: 10,
        turn_timeout_seconds: 10,
        terminal_timeout_seconds: 5,
        terminal_runtime_seconds: 10,
        output_bytes: 65536,
    };
    let assignment = create_assignment(limits)?;
    let workspace = Workspace {
        path: ws_path,
        git_dir: root.path().join("git"),
        home: root.path().join("home"),
    };
    let profile = Profile {
        backend: Backend::RootlessPodman,
        image: "test-image".into(),
    };
    let creds = BTreeMap::new();

    let session = Session {
        client: &client,
        assignment: &assignment,
        workspace: &workspace,
        directory: root.path(),
        home: root.path(),
        profile: &profile,
        credentials: &creds,
    };

    let mut broker = Broker::new(session);
    let session_id = "sess-q3";
    // Invariant: Broker ownership established immediately upon session creation
    broker.session_id = Some(session_id.into());
    broker.session_digest = digest(format!("{}:{session_id}", assignment.attempt_id).as_bytes());

    let (pipe_in_read, pipe_in_write) = tokio::io::duplex(4096);
    let (pipe_out_read, pipe_out_write) = tokio::io::duplex(4096);
    let mut wire = Wire::new(pipe_in_read, pipe_out_write, 1024 * 1024);
    let mut peer_wire = Wire::new(pipe_out_read, pipe_in_write, 1024 * 1024);

    let select_fut = select_model(
        &mut wire,
        &mut broker,
        session_id,
        Some("gemini-3.8-flash-high"),
        Some("gemini-3.8-flash"),
    );

    let peer_fut = async {
        // 1. Peer receives session/set_config_option (e.g. orbit-1 or orbit-3)
        let req1 = peer_wire.read().await?;
        assert_eq!(req1["method"], "session/set_config_option");
        assert_eq!(req1["params"]["configId"], "model");
        assert_eq!(req1["params"]["value"], "gemini-3.8-flash-high");
        let req1_id = req1["id"].clone();

        // 2. Peer emits interleaved session/update notification before response
        peer_wire
            .send(json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": session_id,
                    "update": {
                        "sessionUpdate": "available_commands_update",
                        "availableCommands": []
                    }
                }
            }))
            .await?;

        // 3. Peer responds to req1 with confirmed configOptions
        peer_wire
            .response_ok(
                req1_id,
                json!({
                    "configOptions": [
                        {
                            "id": "model",
                            "currentValue": "gemini-3.8-flash-high"
                        }
                    ]
                }),
            )
            .await?;
        Ok::<(), anyhow::Error>(())
    };

    let (select_res, peer_res) = tokio::join!(select_fut, peer_fut);
    select_res.expect("select_model must succeed with interleaved notification");
    assert!(peer_res.is_ok());
    assert!(!broker.poisoned, "broker must NOT be poisoned");

    // 4. Now send next request (e.g. session/set_mode)
    let next_req_fut = acp_request(
        &mut wire,
        &mut broker,
        "session/set_mode",
        json!({
            "sessionId": session_id,
            "modeId": "yolo"
        }),
    );

    let next_peer_fut = async {
        let req2 = peer_wire.read().await?;
        assert_eq!(req2["method"], "session/set_mode");
        peer_wire.response_ok(req2["id"].clone(), json!({})).await?;
        Ok::<(), anyhow::Error>(())
    };

    let (next_res, next_peer_res) = tokio::join!(next_req_fut, next_peer_fut);
    assert!(
        next_res.is_ok(),
        "subsequent request must succeed without stale response on wire"
    );
    assert!(next_peer_res.is_ok());
    assert!(!broker.poisoned);

    Ok(())
}

#[tokio::test]
async fn test_foreign_session_notification_poisons_broker_and_fails() -> Result<()> {
    let (server_url, _shutdown) = start_mock_server().await;
    let client = Client::new(server_url, "test-token".into())?;

    let root = tempfile::tempdir()?;
    let ws_path = root.path().join("workspace");
    std::fs::create_dir_all(&ws_path)?;

    let limits = Limits {
        prompt_turns: 1,
        broker_calls: 10,
        reported_tool_calls: 10,
        turn_timeout_seconds: 10,
        terminal_timeout_seconds: 5,
        terminal_runtime_seconds: 10,
        output_bytes: 65536,
    };
    let assignment = create_assignment(limits)?;
    let workspace = Workspace {
        path: ws_path,
        git_dir: root.path().join("git"),
        home: root.path().join("home"),
    };
    let profile = Profile {
        backend: Backend::RootlessPodman,
        image: "test-image".into(),
    };
    let creds = BTreeMap::new();

    let session = Session {
        client: &client,
        assignment: &assignment,
        workspace: &workspace,
        directory: root.path(),
        home: root.path(),
        profile: &profile,
        credentials: &creds,
    };

    let mut broker = Broker::new(session);
    let session_id = "sess-owner";
    broker.session_id = Some(session_id.into());
    broker.session_digest = digest(format!("{}:{session_id}", assignment.attempt_id).as_bytes());

    let (pipe_in_read, pipe_in_write) = tokio::io::duplex(4096);
    let (pipe_out_read, pipe_out_write) = tokio::io::duplex(4096);
    let mut wire = Wire::new(pipe_in_read, pipe_out_write, 1024 * 1024);
    let mut peer_wire = Wire::new(pipe_out_read, pipe_in_write, 1024 * 1024);

    let req_fut = acp_request(
        &mut wire,
        &mut broker,
        "session/set_config_option",
        json!({
            "sessionId": session_id,
            "configId": "model",
            "value": "gemini-3.8-flash"
        }),
    );

    let peer_fut = async {
        let req = peer_wire.read().await?;
        assert_eq!(req["method"], "session/set_config_option");

        // Peer sends notification with FOREIGN sessionId
        peer_wire
            .send(json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "sess-foreign",
                    "update": {
                        "sessionUpdate": "available_commands_update",
                        "availableCommands": []
                    }
                }
            }))
            .await?;
        Ok::<(), anyhow::Error>(())
    };

    let (req_res, peer_res) = tokio::join!(req_fut, peer_fut);
    assert!(peer_res.is_ok());
    assert!(
        req_res.is_err(),
        "must fail when foreign notification is received"
    );
    assert!(
        broker.poisoned,
        "broker must be poisoned on foreign session ID"
    );

    Ok(())
}

#[tokio::test]
async fn test_response_id_mismatch_fails_closed() -> Result<()> {
    let (server_url, _shutdown) = start_mock_server().await;
    let client = Client::new(server_url, "test-token".into())?;

    let root = tempfile::tempdir()?;
    let ws_path = root.path().join("workspace");
    std::fs::create_dir_all(&ws_path)?;

    let limits = Limits {
        prompt_turns: 1,
        broker_calls: 10,
        reported_tool_calls: 10,
        turn_timeout_seconds: 10,
        terminal_timeout_seconds: 5,
        terminal_runtime_seconds: 10,
        output_bytes: 65536,
    };
    let assignment = create_assignment(limits)?;
    let workspace = Workspace {
        path: ws_path,
        git_dir: root.path().join("git"),
        home: root.path().join("home"),
    };
    let profile = Profile {
        backend: Backend::RootlessPodman,
        image: "test-image".into(),
    };
    let creds = BTreeMap::new();

    let session = Session {
        client: &client,
        assignment: &assignment,
        workspace: &workspace,
        directory: root.path(),
        home: root.path(),
        profile: &profile,
        credentials: &creds,
    };

    let mut broker = Broker::new(session);
    broker.session_id = Some("sess-id-test".into());

    let (pipe_in_read, pipe_in_write) = tokio::io::duplex(4096);
    let (pipe_out_read, pipe_out_write) = tokio::io::duplex(4096);
    let mut wire = Wire::new(pipe_in_read, pipe_out_write, 1024 * 1024);
    let mut peer_wire = Wire::new(pipe_out_read, pipe_in_write, 1024 * 1024);

    let req_fut = acp_request(
        &mut wire,
        &mut broker,
        "session/test_method",
        json!({"sessionId": "sess-id-test"}),
    );

    let peer_fut = async {
        let req = peer_wire.read().await?;
        assert_eq!(req["id"], "orbit-1");

        // Peer sends response with mismatched ID orbit-999
        peer_wire
            .response_ok(json!("orbit-999"), json!({"status": "ok"}))
            .await?;
        Ok::<(), anyhow::Error>(())
    };

    let (req_res, peer_res) = tokio::join!(req_fut, peer_fut);
    assert!(peer_res.is_ok());
    assert!(
        req_res.is_err(),
        "must fail closed when response ID mismatches"
    );
    let err_str = req_res.unwrap_err().to_string();
    assert!(
        err_str.contains("foreign agent response"),
        "error message should indicate foreign agent response: {err_str}"
    );

    Ok(())
}

#[test]
fn timeout_diagnostic_contains_only_bounded_protocol_state() -> Result<()> {
    let root = tempfile::tempdir()?;
    let client = Client::new("http://127.0.0.1:1".into(), String::new())?;
    let assignment = create_assignment(Limits {
        prompt_turns: 1,
        broker_calls: 10,
        reported_tool_calls: 10,
        turn_timeout_seconds: 10,
        terminal_timeout_seconds: 5,
        terminal_runtime_seconds: 10,
        output_bytes: 65536,
    })?;
    let workspace = Workspace {
        path: root.path().join("workspace"),
        git_dir: root.path().join("git"),
        home: root.path().join("home"),
    };
    let profile = Profile {
        backend: Backend::RootlessPodman,
        image: "sha256:fixture".into(),
    };
    let credentials = BTreeMap::new();
    let session = Session {
        client: &client,
        assignment: &assignment,
        workspace: &workspace,
        directory: root.path(),
        home: root.path(),
        profile: &profile,
        credentials: &credentials,
    };
    let mut broker = Broker::new(session);
    broker.session_id = Some("session-1".into());
    broker.session_digest = "digest".into();
    broker.last_activity_epoch_ms = Some(1000);
    broker.last_activity_kind = Some("read_file".into());
    broker.turn_started_epoch_ms = Some(900);
    broker.pending_model_call = true;
    broker.pending_tool_callbacks = 1;
    let diagnostic = broker.timeout_diagnostic();
    assert!(diagnostic.contains("session_id=session-1"));
    assert!(diagnostic.contains("last_activity_kind=read_file"));
    assert!(diagnostic.contains("pending_model_call=true"));
    assert!(diagnostic.contains("active_tool_callbacks=1"));
    assert!(!diagnostic.contains("Authorization"));
    assert!(!diagnostic.contains("token"));
    Ok(())
}

#[tokio::test]
async fn test_multiple_interleaved_notifications_and_callbacks() -> Result<()> {
    let (server_url, _shutdown) = start_mock_server().await;
    let client = Client::new(server_url, "test-token".into())?;

    let root = tempfile::tempdir()?;
    let ws_path = root.path().join("workspace");
    std::fs::create_dir_all(&ws_path)?;
    std::fs::write(ws_path.join("read_me.txt"), "interleaved test file\n")?;

    let limits = Limits {
        prompt_turns: 1,
        broker_calls: 10,
        reported_tool_calls: 10,
        turn_timeout_seconds: 10,
        terminal_timeout_seconds: 5,
        terminal_runtime_seconds: 10,
        output_bytes: 65536,
    };
    let assignment = create_assignment(limits)?;
    let workspace = Workspace {
        path: ws_path,
        git_dir: root.path().join("git"),
        home: root.path().join("home"),
    };
    let profile = Profile {
        backend: Backend::RootlessPodman,
        image: "test-image".into(),
    };
    let creds = BTreeMap::new();

    let session = Session {
        client: &client,
        assignment: &assignment,
        workspace: &workspace,
        directory: root.path(),
        home: root.path(),
        profile: &profile,
        credentials: &creds,
    };

    let mut broker = Broker::new(session);
    let session_id = "sess-multi";
    broker.session_id = Some(session_id.into());
    broker.session_digest = digest(format!("{}:{session_id}", assignment.attempt_id).as_bytes());
    broker.active = true; // allow callbacks

    let (pipe_in_read, pipe_in_write) = tokio::io::duplex(4096);
    let (pipe_out_read, pipe_out_write) = tokio::io::duplex(4096);
    let mut wire = Wire::new(pipe_in_read, pipe_out_write, 1024 * 1024);
    let mut peer_wire = Wire::new(pipe_out_read, pipe_in_write, 1024 * 1024);

    let req_fut = acp_request(
        &mut wire,
        &mut broker,
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "do something"}]
        }),
    );

    let peer_fut = async {
        let req = peer_wire.read().await?;
        assert_eq!(req["method"], "session/prompt");
        let prompt_req_id = req["id"].clone();

        // 1. Interleaved notification 1: state update
        peer_wire
            .send(json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": session_id,
                    "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": {"type": "text", "text": "thinking"}
                    }
                }
            }))
            .await?;

        // 2. Interleaved notification 2: available commands
        peer_wire
            .send(json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": session_id,
                    "update": {
                        "sessionUpdate": "available_commands_update",
                        "availableCommands": []
                    }
                }
            }))
            .await?;

        // 3. Interleaved client callback request: fs/read_text_file
        let callback_id = json!("call-1");
        peer_wire
            .send(json!({
                "jsonrpc": "2.0",
                "id": callback_id,
                "method": "fs/read_text_file",
                "params": {
                    "sessionId": session_id,
                    "path": format!("{WORKSPACE}/read_me.txt"),
                    "line": 1,
                    "limit": 10
                }
            }))
            .await?;

        // Read callback response from Orbit broker
        let cb_resp = peer_wire.read().await?;
        assert_eq!(cb_resp["id"], callback_id);
        assert_eq!(cb_resp["result"]["content"], "interleaved test file\n");

        // 4. Finally, send response to original request
        peer_wire
            .response_ok(prompt_req_id, json!({"stopReason": "end_turn"}))
            .await?;

        Ok::<(), anyhow::Error>(())
    };

    let (req_res, peer_res) = tokio::join!(req_fut, peer_fut);
    assert!(peer_res.is_ok());
    assert!(
        req_res.is_ok(),
        "acp_request must succeed after multiple interleaved notifications and callbacks"
    );
    let resp = req_res.unwrap();
    assert_eq!(resp["stopReason"], "end_turn");
    assert!(!broker.poisoned);
    assert_eq!(broker.tool_calls, 1);
    assert_eq!(broker.tool_successes, 1);

    Ok(())
}
