use anyhow::Result;
use orbit::{
    acp_contract::{Record, RecordBatch, RecordKind},
    acp_process::AuthLease,
    acp_runtime::Runtime,
    agent::Usage,
};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;

fn runtime(path: &std::path::Path) -> Result<Runtime> {
    let raw: Value = serde_json::from_str(include_str!("fixtures/acp-contract.json"))?;
    let mut value = json!({"binding_name":"codex-fixture","binding":raw["binding"],
        "launch":{"adapter":"acp","image":format!("sha256:{}","b".repeat(64)),"command":["/bin/true"],
            "agent_name":"fixture","agent_version":"1","binary_revision":"1","cpu_millis":1000,"memory_mib":128,"network":"none"},
        "auth":{"path":path,"source":"fixture-auth","owner":"fixture-owner","account_class":"fixture","files":{"auth.json":".agent/auth.json"}}});
    let launch: orbit::acp_runtime::Launch = serde_json::from_value(value["launch"].clone())?;
    value["binding"]["acp"]["launch_digest"] = json!(launch.digest()?);
    Ok(serde_json::from_value(value)?)
}
#[test]
fn acp_auth_lock_refresh_and_quarantine_are_private_and_fail_closed() -> Result<()> {
    let root = tempfile::tempdir()?;
    let auth = root.path().join("auth");
    std::fs::create_dir(&auth)?;
    std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o700))?;
    std::fs::write(auth.join("auth.json"), "fixture-old")?;
    std::fs::set_permissions(
        auth.join("auth.json"),
        std::fs::Permissions::from_mode(0o600),
    )?;
    let runtime = runtime(&auth)?;
    runtime.validate()?;
    let lease = AuthLease::acquire(&runtime)?;
    assert!(AuthLease::acquire(&runtime).is_err());
    let home = root.path().join("home");
    lease.stage(&home, "orbit-fixture", "attempt")?;
    std::fs::write(home.join(".agent/auth.json"), "fixture-refreshed")?;
    lease.finish(&home)?;
    assert_eq!(
        std::fs::read_to_string(auth.join("auth.json"))?,
        "fixture-refreshed"
    );
    assert!(std::fs::read(home.join(".agent/auth.json"))?.is_empty());
    assert!(!auth.join(".orbit-acp-active.json").exists());
    let lease = AuthLease::acquire(&runtime)?;
    let home = root.path().join("unsafe-home");
    lease.stage(&home, "orbit-uncertain", "attempt")?;
    std::fs::set_permissions(
        home.join(".agent/auth.json"),
        std::fs::Permissions::from_mode(0o644),
    )?;
    assert!(lease.finish(&home).is_err());
    assert!(AuthLease::acquire(&runtime).is_err());
    assert_eq!(
        std::fs::read_to_string(auth.join("auth.json"))?,
        "fixture-refreshed"
    );
    Ok(())
}
#[test]
fn acp_records_are_bounded_ordered_idempotent_and_cumulative() -> Result<()> {
    let raw: Value = serde_json::from_str(include_str!("fixtures/acp-contract.json"))?;
    let limits = serde_json::from_value(raw["agent"]["acp_limits"].clone())?;
    let mut usage = Usage::default();
    let mut batch = RecordBatch {
        attempt_id: orbit::model::id(),
        session_digest: "a".repeat(64),
        sequence: 0,
        records: vec![Record {
            kind: RecordKind::Started,
            digest: "b".repeat(64),
            output_bytes: 0,
            reported_tool_calls: 0,
        }],
    };
    assert!(usage.record_acp(&limits, &batch)?);
    assert!(!usage.record_acp(&limits, &batch)?);
    batch.records[0].digest = "c".repeat(64);
    assert!(usage.record_acp(&limits, &batch).is_err());
    batch.sequence = 2;
    batch.records[0].kind = RecordKind::Update;
    assert!(usage.record_acp(&limits, &batch).is_err());
    batch.sequence = 1;
    batch.records[0].output_bytes = 65535;
    batch.records[0].reported_tool_calls = 8;
    assert!(usage.record_acp(&limits, &batch)?);
    batch.sequence = 2;
    assert!(usage.record_acp(&limits, &batch).is_err());
    batch.records[0].output_bytes = 0;
    batch.records[0].reported_tool_calls = 0;
    batch.records[0].kind = RecordKind::Completed;
    assert!(usage.record_acp(&limits, &batch)?);
    assert!(!usage.record_acp(&limits, &batch)?);
    batch.sequence = 3;
    assert!(usage.record_acp(&limits, &batch).is_err());
    batch.attempt_id = orbit::model::id();
    batch.session_digest = "d".repeat(64);
    batch.sequence = 0;
    batch.records[0].kind = RecordKind::Started;
    assert!(usage.record_acp(&limits, &batch)?);
    batch.sequence = 1;
    batch.records[0].kind = RecordKind::Update;
    batch.records[0].output_bytes = 2;
    assert!(usage.record_acp(&limits, &batch).is_err());
    Ok(())
}
#[tokio::test]
async fn acp_wire_bounds_frames_total_messages_and_protocol_envelopes() -> Result<()> {
    use tokio::io::AsyncWriteExt;
    for payload in [
        b"[]\n".to_vec(),
        b"{\"id\":1,\"result\":{}}\n".to_vec(),
        vec![b'x'; 1024 * 1024 + 1],
    ] {
        let (reader, mut writer) = tokio::io::duplex(2048);
        let task = tokio::spawn(async move {
            let _ = writer.write_all(&payload).await;
        });
        let mut wire = orbit::acp_wire::Wire::new(reader, tokio::io::sink(), 2 * 1024 * 1024);
        assert!(wire.read().await.is_err());
        task.abort();
    }
    let frame = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n";
    let mut wire = orbit::acp_wire::Wire::new(frame.as_slice(), tokio::io::sink(), 2);
    assert!(wire.read().await.is_err());
    let raw = b"{\"id\":1,\"result\":{}}\n";
    let mut wire = orbit::acp_wire::Wire::new(raw.as_slice(), tokio::io::sink(), 100).codex();
    let result = wire.read().await?;
    assert!(orbit::acp_wire::Wire::result(result, &json!("foreign")).is_err());
    Ok(())
}
#[test]
fn acp_cleanup_receipt_binds_request_and_attempt_and_terminal_utf8_is_bounded() -> Result<()> {
    let root = tempfile::tempdir()?;
    let request = root.path().join("request.json");
    orbit::acp_process::write_cleanup(&request, "attempt", 1)?;
    assert_eq!(
        orbit::acp_process::read_cleanup(&request, Some("attempt"))?,
        1
    );
    assert!(orbit::acp_process::read_cleanup(&request, Some("foreign")).is_err());
    assert!(orbit::acp_process::write_cleanup(&request, "attempt", 1).is_err());
    let diagnostic_request = root.path().join("diagnostic.json");
    std::fs::write(&diagnostic_request, b"{}")?;
    orbit::acp_process::write_cleanup_diagnostic(
        &diagnostic_request,
        "attempt",
        125,
        "container_startup",
        Some("sha256:abc"),
        Some("image not known"),
    )?;
    let diagnostic =
        orbit::acp_process::read_cleanup_diagnostic(&diagnostic_request, Some("attempt"))?.unwrap();
    assert!(diagnostic.contains("container_startup"));
    assert!(diagnostic.contains("image not known"));
    let output = orbit::acp_terminal::Output {
        bytes: vec![0xff, 0xfe, b'a'],
        ..Default::default()
    };
    assert!(output.text().len() <= 3);
    Ok(())
}

#[test]
fn acp_transcript_and_report_reject_unaccepted_or_misattributed_evidence() -> Result<()> {
    let raw: Value = serde_json::from_str(include_str!("fixtures/acp-contract.json"))?;
    let spec: orbit::agent::AgentSpec = serde_json::from_value(raw["agent"].clone())?;
    let binding: orbit::agent::Binding = serde_json::from_value(raw["binding"].clone())?;
    let mut report = orbit::agent::AgentReport {
        attempt_id: "attempt".into(),
        binding_digest: orbit::model::digest(&serde_json::to_vec(&binding)?),
        delegation_inputs: vec![],
        output: json!({"acp":{"launch_digest":binding.acp.as_ref().unwrap().launch_digest,"model":binding.model,"model_attribution":"agent_confirmed_exact",
            "stop_reason":"end_turn","accounting":"execution_only","tokens":null,"cost_microusd":null}}),
    };
    spec.validate_report(&report, "attempt", &binding)?;
    report.output["acp"]["model"] = json!("unconfirmed-model");
    assert!(spec.validate_report(&report, "attempt", &binding).is_err());
    let batch = RecordBatch {
        attempt_id: orbit::model::id(),
        session_digest: "a".repeat(64),
        sequence: 0,
        records: vec![Record {
            kind: RecordKind::Started,
            digest: "b".repeat(64),
            output_bytes: 0,
            reported_tool_calls: 0,
        }],
    };
    let bytes = serde_json::to_vec(&batch)?;
    let session = orbit::acp_contract::SessionUsage {
        attempt_id: batch.attempt_id.clone(),
        batches: vec![orbit::model::digest(&bytes)],
        output_bytes: 0,
        reported_tool_calls: 0,
        completed: false,
    };
    orbit::acp_contract::verify_transcript(&bytes, &batch.session_digest, &session)?;
    assert!(orbit::acp_contract::verify_transcript(&bytes, &"c".repeat(64), &session).is_err());
    assert!(orbit::acp_contract::verify_transcript(b"", &batch.session_digest, &session).is_err());
    Ok(())
}

#[test]
fn acp_launch_digest_cli_uses_canonical_policy_without_api_credentials() -> Result<()> {
    let root = tempfile::tempdir()?;
    let runtime = runtime(root.path())?;
    let path = root.path().join("launch.json");
    std::fs::write(&path, serde_json::to_vec(&runtime.launch)?)?;
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_orbit"))
        .args(["acp-launch-digest", "--config"])
        .arg(&path)
        .env("ORBIT_TOKEN_FILE", root.path().join("missing-token"))
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value["launch_digest"], runtime.launch.digest()?);
    Ok(())
}

#[test]
fn acp_example_registry_pins_launch_scope_and_combined_resources() -> Result<()> {
    use orbit::model::*;
    use std::collections::BTreeMap;
    let mut raw: Value = serde_json::from_str(include_str!("../examples/acp-worker.json"))?;
    raw["acp_agents"][0]["launch"]["image"] = json!(format!("sha256:{}", "b".repeat(64)));
    let launch: orbit::acp_runtime::Launch =
        serde_json::from_value(raw["acp_agents"][0]["launch"].clone())?;
    raw["acp_agents"][0]["binding"]["acp"]["launch_digest"] = json!(launch.digest()?);
    let worker: orbit::execution::WorkerConfig = serde_json::from_value(raw.clone())?;
    worker.validate()?;
    let runtime = &worker.acp_agents[0];
    let definition = Definition::parse(
        &include_str!("../examples/acp-coding.yaml")
            .replace("REPLACE_WITH_FULL_COMMIT_ID", &"a".repeat(40)),
    )?;
    let server: orbit::api::Config =
        serde_json::from_str(include_str!("../examples/server-remote-coding.json"))?;
    let plan = Plan::compile_with_execution(
        definition,
        server.repositories["approved-repository"].clone(),
        &BTreeMap::from([(runtime.binding_name.clone(), runtime.binding.clone())]),
        &server.execution_profiles,
    )?;
    let mut assignment = Assignment {
        run_id: id(),
        task_id: id(),
        attempt_id: id(),
        generation: 1,
        workspace_id: id(),
        lease_token: String::new(),
        lease_expires_at: 0,
        heartbeat_interval: 1,
        deadline_at: 0,
        plan,
        step: "code".into(),
        input_artifacts: vec![],
        idempotency_key: id(),
        gpu_devices: vec![],
        agent_binding_digest: Some(digest(&serde_json::to_vec(&runtime.binding)?)),
        execution_id: None,
    };
    runtime.authorize(&assignment)?;
    worker.authorize(&assignment)?;
    assignment
        .plan
        .definition
        .steps
        .get_mut("code")
        .unwrap()
        .resources
        .as_mut()
        .unwrap()
        .cpu_millis = 1000;
    assert!(runtime.authorize(&assignment).is_err());
    assignment
        .plan
        .definition
        .steps
        .get_mut("code")
        .unwrap()
        .resources
        .as_mut()
        .unwrap()
        .cpu_millis = 2000;
    assignment.plan.scope = Some(orbit::governance::Scope::parse("org/project/dev")?);
    assert!(runtime.authorize(&assignment).is_err());
    let mut allowed = runtime.clone();
    allowed.auth.scopes = vec![assignment.plan.scope.clone().unwrap()];
    allowed.authorize(&assignment)?;
    allowed.launch.network = orbit::acp_runtime::AgentNetwork::None;
    assert!(allowed.validate().is_err());
    let duplicate = raw["acp_agents"][0].clone();
    raw["acp_agents"].as_array_mut().unwrap().push(duplicate);
    assert!(
        serde_json::from_value::<orbit::execution::WorkerConfig>(raw)?
            .validate()
            .is_err()
    );
    Ok(())
}

#[test]
fn acp_antigravity_launch_and_isolated_gemini_auth_validate() -> Result<()> {
    let root = tempfile::tempdir()?;
    let auth = root.path().join("auth");
    std::fs::create_dir(&auth)?;
    std::fs::write(auth.join("acp_token.json"), "{}")?;
    std::fs::write(auth.join("settings.json"), "{}")?;

    let mut raw: Value = serde_json::from_str(include_str!("fixtures/acp-contract.json"))?;
    raw["binding"]["acp"]["agent_id"] = json!("antigravity-acp");
    raw["binding"]["acp"]["agent_revision"] = json!("agy_acp_server_1.1.1");
    raw["binding"]["model"] = json!("gemini-3.7-flash-high");

    let launch_val = json!({
        "adapter": "antigravity",
        "image": format!("sha256:{}", "c".repeat(64)),
        "command": ["/opt/antigravity/agy_acp_server.par"],
        "agent_name": "antigravity-acp",
        "agent_version": "agy_acp_server_1.1.1",
        "binary_revision": "agy_acp_server_1.1.1",
        "cpu_millis": 2000,
        "memory_mib": 4096,
        "network": "host"
    });
    let launch: orbit::acp_runtime::Launch = serde_json::from_value(launch_val.clone())?;
    launch.validate()?;

    let mut runtime_val = json!({
        "binding_name": "antigravity-fixture",
        "binding": raw["binding"],
        "launch": launch_val,
        "auth": {
            "path": auth,
            "source": "fixture-auth",
            "owner": "fixture-owner",
            "account_class": "fixture",
            "files": {
                "acp_token.json": ".gemini/antigravity-acp/acp_token.json",
                "settings.json": ".gemini/antigravity-acp/settings.json"
            }
        }
    });
    runtime_val["binding"]["acp"]["launch_digest"] = json!(launch.digest()?);

    let runtime: orbit::acp_runtime::Runtime = serde_json::from_value(runtime_val.clone())?;
    runtime.validate()?;

    // Unconfined auth files outside .gemini/ must be rejected
    let mut bad_auth_runtime = runtime_val.clone();
    bad_auth_runtime["auth"]["files"] = json!({
        "acp_token.json": "unconfined/acp_token.json"
    });
    let bad_runtime: orbit::acp_runtime::Runtime = serde_json::from_value(bad_auth_runtime)?;
    assert!(bad_runtime.validate().is_err());

    // Wrong agent_name in launch must be rejected
    let mut bad_launch_runtime = runtime_val.clone();
    bad_launch_runtime["launch"]["agent_name"] = json!("wrong-agent");
    let bad_runtime: orbit::acp_runtime::Runtime = serde_json::from_value(bad_launch_runtime)?;
    assert!(bad_runtime.validate().is_err());

    Ok(())
}

#[test]
fn acp_antigravity_example_registry_pins_launch_scope_and_combined_resources() -> Result<()> {
    use orbit::model::*;
    use std::collections::BTreeMap;
    let mut raw: Value = serde_json::from_str(include_str!("../examples/antigravity-worker.json"))?;
    raw["acp_agents"][0]["launch"]["image"] = json!(format!("sha256:{}", "b".repeat(64)));
    let launch: orbit::acp_runtime::Launch =
        serde_json::from_value(raw["acp_agents"][0]["launch"].clone())?;
    raw["acp_agents"][0]["binding"]["acp"]["launch_digest"] = json!(launch.digest()?);
    let worker: orbit::execution::WorkerConfig = serde_json::from_value(raw.clone())?;
    worker.validate()?;
    let runtime = &worker.acp_agents[0];
    let definition = Definition::parse(
        &include_str!("../examples/antigravity-coding.yaml")
            .replace("REPLACE_WITH_FULL_COMMIT_ID", &"a".repeat(40)),
    )?;
    let server: orbit::api::Config =
        serde_json::from_str(include_str!("../examples/server-remote-coding.json"))?;
    let plan = Plan::compile_with_execution(
        definition,
        server.repositories["approved-repository"].clone(),
        &BTreeMap::from([(runtime.binding_name.clone(), runtime.binding.clone())]),
        &server.execution_profiles,
    )?;
    let assignment = Assignment {
        run_id: id(),
        task_id: id(),
        attempt_id: id(),
        generation: 1,
        workspace_id: id(),
        lease_token: String::new(),
        lease_expires_at: 0,
        heartbeat_interval: 1,
        deadline_at: 0,
        plan,
        step: "code".into(),
        input_artifacts: vec![],
        idempotency_key: id(),
        gpu_devices: vec![],
        agent_binding_digest: Some(digest(&serde_json::to_vec(&runtime.binding)?)),
        execution_id: None,
    };
    runtime.authorize(&assignment)?;
    worker.authorize(&assignment)?;
    Ok(())
}
