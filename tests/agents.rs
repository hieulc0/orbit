use orbit::{
    agent::*,
    mcp::{Session, tools},
    model::*,
    worker::Client,
};
use serde_json::json;
use std::collections::BTreeMap;

fn bindings() -> BTreeMap<String, Binding> {
    serde_json::from_str(include_str!("../examples/agent-bindings.json")).unwrap()
}
#[test]
fn agents_pin_authorized_bindings_and_restrict_delegation() {
    let def = Definition::parse(include_str!("../examples/agent.yaml")).unwrap();
    assert!(Plan::compile(def.clone(), RepositoryBinding::none()).is_err());
    let binding = bindings();
    let plan = Plan::compile_with_agents(def.clone(), RepositoryBinding::none(), &binding).unwrap();
    let mut changed = binding.clone();
    changed.get_mut("local-agent").unwrap().model = "fixture-model/revision-2".into();
    assert_ne!(
        plan.digest,
        Plan::compile_with_agents(def.clone(), RepositoryBinding::none(), &changed)
            .unwrap()
            .digest
    );
    for field in ["tools", "permissions"] {
        let mut raw = serde_json::to_value(&def).unwrap();
        raw["steps"]["planner"]["agent"][field] = json!(["forbidden"]);
        assert!(
            Plan::compile_with_agents(
                serde_json::from_value(raw).unwrap(),
                RepositoryBinding::none(),
                &binding
            )
            .is_err()
        );
    }
    let mut bad = def.clone();
    bad.steps
        .get_mut("planner")
        .unwrap()
        .agent
        .as_mut()
        .unwrap()
        .max_delegations = 3;
    assert!(bad.validate().is_err());
    let mut bad = def;
    bad.steps
        .get_mut("review")
        .unwrap()
        .approval
        .as_mut()
        .unwrap()
        .assignees
        .clear();
    assert!(bad.validate().is_err());
}
#[test]
fn budget_reservations_are_bounded_and_replay_safe() {
    let def = Definition::parse(include_str!("../examples/agent.yaml")).unwrap();
    let spec = def.steps["planner"].agent.as_ref().unwrap();
    let mut usage = Usage::default();
    let mut call = CallReservation {
        request_digest: None,
        call_id: "call-1".into(),
        tokens: 600,
        cost_microusd: 5000,
        tool: None,
        permissions: vec![],
    };
    assert!(usage.reserve(spec, &call).unwrap());
    assert!(!usage.reserve(spec, &call).unwrap());
    call.tokens = 1;
    assert!(usage.reserve(spec, &call).is_err());
    call.call_id = "call-2".into();
    call.tokens = 500;
    assert!(usage.reserve(spec, &call).is_err());
    call.tokens = u64::MAX;
    assert!(usage.reserve(spec, &call).is_err());
    call.tokens = 1;
    call.tool = Some("shell".into());
    assert!(usage.reserve(spec, &call).is_err());
    call.tool = Some("summarize".into());
    call.permissions = vec!["network.write".into()];
    assert!(usage.reserve(spec, &call).is_err());
    call.permissions = vec!["context.read".into()];
    assert!(usage.reserve(spec, &call).unwrap());
    assert_eq!(usage.tokens, 601);
}
#[test]
fn agent_reports_enforce_output_provenance_and_bounds() {
    let def = Definition::parse(include_str!("../examples/agent.yaml")).unwrap();
    let spec = def.steps["planner"].agent.as_ref().unwrap();
    let bindings = bindings();
    let binding = &bindings["local-agent"];
    let mut report = AgentReport {
        attempt_id: "attempt".into(),
        binding_digest: digest(&serde_json::to_vec(binding).unwrap()),
        output: json!({"ok":true}),
        delegation_inputs: vec!["one".into()],
    };
    spec.validate_report(&report, "attempt", binding).unwrap();
    assert!(spec.validate_report(&report, "other", binding).is_err());
    report.output = json!("not an object");
    assert!(spec.validate_report(&report, "attempt", binding).is_err());
    report.output = json!({});
    report.delegation_inputs = vec!["one".into(); 3];
    assert!(spec.validate_report(&report, "attempt", binding).is_err());
}
#[test]
fn mcp_stdio_emits_only_protocol_messages_and_exits_on_eof() {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    let mut child = Command::new(env!("CARGO_BIN_EXE_orbit"))
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for message in [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"validate_definition","arguments":{"yaml":include_str!("../examples/agent.yaml")}}}),
    ] {
        writeln!(input, "{message}").unwrap();
    }
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let records: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(records.len(), 3);
    assert_eq!(records[0]["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(records[2]["result"]["isError"], false);
}
#[tokio::test]
async fn mcp_lifecycle_validation_and_no_approval_tool() {
    let client = Client::new("http://127.0.0.1:1".into(), "".into()).unwrap();
    let mut session = Session::default();
    let request =
        |id, method, params| json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
    assert_eq!(
        session
            .handle(&client, request(1, "tools/list", json!({})))
            .await
            .unwrap()["error"]["code"],
        -32600
    );
    let init = request(
        2,
        "initialize",
        json!({"protocolVersion":"future","capabilities":{},"clientInfo":{"name":"test","version":"1"}}),
    );
    assert_eq!(
        session.handle(&client, init).await.unwrap()["result"]["protocolVersion"],
        "2025-11-25"
    );
    assert!(
        session
            .handle(
                &client,
                json!({"jsonrpc":"2.0","method":"notifications/initialized"})
            )
            .await
            .is_none()
    );
    assert!(
        !tools()
            .iter()
            .any(|t| t["name"].as_str().unwrap().contains("approv"))
    );
    assert_eq!(
        session
            .handle(&client, request(3, "tools/list", json!({})))
            .await
            .unwrap()["result"]["tools"]
            .as_array()
            .unwrap()
            .len(),
        10
    );
    let result = session.handle(&client,request(4,"tools/call",json!({"name":"validate_definition","arguments":{"yaml":include_str!("../examples/agent.yaml")}}))).await.unwrap();
    assert_eq!(result["result"]["isError"], false);
    let result = session
        .handle(
            &client,
            request(
                5,
                "tools/call",
                json!({"name":"get_run","arguments":{"run_id":"../limits"}}),
            ),
        )
        .await
        .unwrap();
    assert_eq!(result["result"]["isError"], true);
    assert_eq!(
        session.handle(&client, json!([])).await.unwrap()["error"]["code"],
        -32600
    );
}
