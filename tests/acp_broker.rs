use anyhow::Result;
use orbit::{
    acp_broker::Broker,
    acp_contract::Limits,
    acp_runtime::{WORKSPACE, select_model},
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
