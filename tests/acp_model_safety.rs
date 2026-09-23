use anyhow::Result;
use orbit::{
    acp_broker::Broker,
    acp_runtime::select_model,
    acp_wire::Wire,
    coding_agent::Session,
    execution::{Backend, Profile},
    model::*,
    repository::Workspace,
    worker::Client,
};
use serde_json::json;
use std::collections::BTreeMap;

async fn scenario(mode: &str) -> Result<()> {
    let root = tempfile::tempdir()?;
    let client = Client::new("http://127.0.0.1:1".into(), "unused".into())?;
    let assignment = Assignment {
        run_id: id(),
        task_id: id(),
        attempt_id: id(),
        generation: 1,
        workspace_id: id(),
        lease_token: "unused".into(),
        lease_expires_at: 9999999999,
        heartbeat_interval: 30,
        deadline_at: 9999999999,
        plan: Plan {
            definition: Definition::parse(include_str!("../examples/agent.yaml"))?,
            digest: "a".repeat(64),
            repository: RepositoryBinding::none(),
            agent_bindings: BTreeMap::new(),
            execution_profiles: BTreeMap::new(),
            scope: None,
        },
        step: "planner".into(),
        input_artifacts: vec![],
        idempotency_key: id(),
        gpu_devices: vec![],
        agent_binding_digest: None,
        execution_id: None,
    };
    let workspace = Workspace {
        path: root.path().into(),
        git_dir: root.path().join(".git"),
        home: root.path().into(),
    };
    let profile = Profile {
        backend: Backend::RootlessPodman,
        image: "unused".into(),
    };
    let credentials = BTreeMap::new();
    let mut broker = Broker::new(Session {
        client: &client,
        assignment: &assignment,
        workspace: &workspace,
        directory: root.path(),
        home: root.path(),
        profile: &profile,
        credentials: &credentials,
    });
    let (local, remote) = tokio::io::duplex(4096);
    let (read, write) = tokio::io::split(local);
    let (peer_read, peer_write) = tokio::io::split(remote);
    let mut wire = Wire::new(read, write, 65536);
    let mut peer = Wire::new(peer_read, peer_write, 65536);
    let selection = select_model(
        &mut wire,
        &mut broker,
        "session",
        Some("requested"),
        Some("old"),
    );
    let peer_response = async {
        let request = peer.read().await?;
        assert_eq!(request["method"], "session/set_config_option");
        match mode {
            "foreign" => peer.response_ok(json!("stale-request"), json!({})).await?,
            "timeout" => {
                // Keep the old request unanswered until its actual policy deadline.
                tokio::time::sleep(std::time::Duration::from_millis(10100)).await;
            }
            "unconfirmed" => {
                peer.response_error(request["id"].clone(), -32601, "unsupported")
                    .await?;
                let fallback = peer.read().await?;
                assert_eq!(fallback["method"], "session/set_model");
                peer.response_ok(fallback["id"].clone(), json!({})).await?;
            }
            _ => unreachable!(),
        }
        // A failed/expired selection must not start another protocol operation.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), peer.read())
                .await
                .is_err()
        );
        Ok::<_, anyhow::Error>(())
    };
    let (result, peer_result) = tokio::join!(selection, peer_response);
    peer_result?;
    let error = result
        .expect_err("unsafe model selection must fail closed")
        .to_string();
    match mode {
        "foreign" => assert!(error.contains("foreign agent response")),
        "timeout" => assert!(error.contains("protocol outcome unconfirmed")),
        "unconfirmed" => assert!(error.contains("could not be activated or confirmed")),
        _ => unreachable!(),
    }
    assert_eq!(broker.tool_calls, 0);
    Ok(())
}

#[tokio::test]
async fn foreign_model_response_does_not_dispatch_fallback() -> Result<()> {
    scenario("foreign").await
}

#[tokio::test]
async fn model_selection_timeout_does_not_reuse_pending_wire() -> Result<()> {
    scenario("timeout").await
}

#[tokio::test]
async fn empty_model_acknowledgement_is_not_actual_model_evidence() -> Result<()> {
    scenario("unconfirmed").await
}
