use anyhow::Result;
use orbit::{
    agent::{AgentSpec, Binding, CallReceipt, CallReservation, Usage},
    model::*,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn fixture() -> (Value, Binding, AgentSpec) {
    let raw: Value = serde_json::from_str(include_str!("fixtures/acp-contract.json")).unwrap();
    let binding = serde_json::from_value(raw["binding"].clone()).unwrap();
    let agent = serde_json::from_value(raw["agent"].clone()).unwrap();
    (raw, binding, agent)
}

#[test]
fn acp_contracts_reject_unenforceable_accounting_permissions_and_policies() -> Result<()> {
    let (raw, binding, spec) = fixture();
    spec.authorize(&binding)?;
    for (pointer, value) in [
        ("/acp/protocol_version", json!(2)),
        ("/acp/launch_digest", json!("latest")),
        ("/acp/auth/source", json!("/home/private/auth")),
        ("/acp/auth/mode", json!("interactive")),
        ("/acp/security_profile", json!("sandboxed")),
        ("/acp/model_policy", json!("best_effort")),
        ("/acp/accounting", json!("billed_tokens")),
        ("/max_budget/tokens", json!(0)),
        ("/max_budget/cost_microusd", json!(0)),
        ("/model", Value::Null),
        ("/tools/shell", json!("orbit.workspace.shell/v1")),
        ("/max_delegations", json!(1)),
    ] {
        let mut candidate = raw["binding"].clone();
        let (prefix, key) = pointer.rsplit_once('/').unwrap();
        candidate.pointer_mut(prefix).unwrap()[key] = value;
        assert!(
            serde_json::from_value::<Binding>(candidate)
                .and_then(|b| b.validate().map_err(serde::de::Error::custom))
                .is_err(),
            "{pointer}"
        );
    }
    for (pointer, value) in [
        ("/acp_limits/prompt_turns", json!(3)),
        ("/acp_limits/output_bytes", json!(1)),
        ("/acp_limits/terminal_timeout_seconds", json!(11)),
        ("/permissions", json!(["workspace.read"])),
        ("/tools", json!(["native_shell"])),
        ("/budget/tokens", json!(1)),
        ("/budget/cost_microusd", json!(0)),
        ("/max_delegations", json!(1)),
        ("/acp_limits", Value::Null),
    ] {
        let mut candidate = raw["agent"].clone();
        let (prefix, key) = pointer.rsplit_once('/').unwrap();
        candidate.pointer_mut(prefix).unwrap()[key] = value;
        assert!(
            serde_json::from_value::<AgentSpec>(candidate)?
                .authorize(&binding)
                .is_err(),
            "{pointer}"
        );
    }
    let mut configured = raw["binding"].clone();
    configured["acp"]["model_policy"] = json!("agent_configured");
    assert!(
        serde_json::from_value::<Binding>(configured.clone())?
            .validate()
            .is_err()
    );
    configured.as_object_mut().unwrap().remove("model");
    serde_json::from_value::<Binding>(configured)?.validate()?;
    let mut unknown = raw["agent"].clone();
    unknown["acp_limits"]["unlimited"] = json!(true);
    assert!(serde_json::from_value::<AgentSpec>(unknown).is_err());
    Ok(())
}

#[test]
fn acp_referenced_and_nested_bindings_pin_policy_without_changing_legacy_bytes() -> Result<()> {
    let (_, binding, spec) = fixture();
    let mut definition = Definition::parse(
        &include_str!("../examples/remote-coding.yaml")
            .replace("REPLACE_WITH_FULL_COMMIT_ID", &"a".repeat(40)),
    )?;
    definition.steps.get_mut("code").unwrap().agent = Some(spec);
    let server: orbit::api::Config =
        serde_json::from_str(include_str!("../examples/server-remote-coding.json"))?;
    let bindings = BTreeMap::from([("codex-fixture".into(), binding.clone())]);
    let compile = |def, bindings: &BTreeMap<String, Binding>| {
        Plan::compile_with_execution(
            def,
            server.repositories["approved-repository"].clone(),
            bindings,
            &server.execution_profiles,
        )
    };
    let plan = compile(definition.clone(), &bindings)?;
    let mut policy = orbit::governance::Policy {
        capabilities: definition.steps.values().map(|s| s.uses.clone()).collect(),
        repository_ids: vec![definition.inputs.repository_id.clone()],
        agent_bindings: vec!["codex-fixture".into()],
        max_resources: orbit::compute::Resources {
            cpu_millis: 1024000,
            memory_mib: 4194304,
            gpu: 64,
        },
        max_agent_budget: Some(serde_json::from_value(json!({"calls":100}))?),
        max_concurrency: 256,
    };
    policy.validate_definition(&definition)?;
    policy.max_agent_budget = Some(serde_json::from_value(
        json!({"tokens":1000000,"cost_microusd":1000000,"calls":100}),
    )?);
    assert!(policy.validate_definition(&definition).is_err());
    policy.max_agent_budget = Some(serde_json::from_value(json!({"calls":1}))?);
    assert!(policy.validate_definition(&definition).is_err());
    let mut unrelated = bindings.clone();
    unrelated.insert("unused".into(), binding);
    assert_eq!(compile(definition.clone(), &unrelated)?.digest, plan.digest);
    unrelated
        .get_mut("codex-fixture")
        .unwrap()
        .acp
        .as_mut()
        .unwrap()
        .auth
        .owner = "another-owner".into();
    assert_ne!(compile(definition.clone(), &unrelated)?.digest, plan.digest);
    let nested: Definition =
        serde_json::from_value(json!({"apiVersion":"orbit/v1", "kind":"Definition",
        "metadata":{"name":"nested-acp"}, "inputs":definition.inputs, "steps":{"child":{
            "uses":"engine.child", "recovery_policy":"restart_from_inputs", "max_attempts":1,
            "timeout_seconds":120,"retry_backoff_seconds":0,"definition":definition}}}))?;
    assert_ne!(
        compile(nested.clone(), &bindings)?.digest,
        compile(nested, &unrelated)?.digest
    );
    definition.steps.get_mut("code").unwrap().uses = "agent.run".into();
    assert!(definition.validate().is_err());

    let legacy = r#"{"model":"fixture-v1","runtime":"agent.fixture-v1","tools":{},"permissions":[],"max_budget":{"tokens":100,"cost_microusd":0,"calls":1},"max_delegations":0}"#;
    let binding: Binding = serde_json::from_str(legacy)?;
    binding.validate()?;
    assert_eq!(serde_json::to_string(&binding)?, legacy);
    let legacy_call =
        r#"{"call_id":"old-call","tokens":0,"cost_microusd":0,"tool":null,"permissions":[]}"#;
    assert_eq!(
        serde_json::to_string(&serde_json::from_str::<CallReservation>(legacy_call)?)?,
        legacy_call
    );
    assert_eq!(
        serde_json::to_string(&Usage::default())?,
        r#"{"tokens":0,"cost_microusd":0,"reservations":{}}"#
    );
    let schema = serde_json::to_value(schemars::schema_for!(Definition))?;
    assert!(
        schema["$defs"]["AgentSpec"]["properties"]
            .get("acp_limits")
            .is_some()
    );
    assert_eq!(schema["$defs"]["Budget"]["required"], json!(["calls"]));
    Ok(())
}

#[test]
fn acp_charges_are_retained_bounded_nullable_and_replay_safe() -> Result<()> {
    let (_, _, spec) = fixture();
    let mut usage = Usage::default();
    let mut call: CallReservation = serde_json::from_value(json!({"call_id":"attempt-prompt-0",
        "tool":null,"permissions":[], "request_digest":"b".repeat(64), "acp_charge":{"kind":"prompt"}}))?;
    assert!(usage.reserve(&spec, &call)?);
    assert!(!usage.reserve(&spec, &call)?);
    assert!(usage.pending_model_call());
    assert_eq!(serde_json::to_value(&usage)?["tokens"], Value::Null);
    assert_eq!(serde_json::to_value(&usage)?["cost_microusd"], Value::Null);
    let receipt = CallReceipt {
        call_id: call.call_id.clone(),
        attempt_id: "attempt".into(),
        result_digest: "c".repeat(64),
        external_id: None,
    };
    assert!(usage.finish(&receipt)?);
    assert!(!usage.finish(&receipt)?);
    assert!(!usage.pending_model_call());
    call.call_id = "retry-prompt-0".into();
    assert!(usage.reserve(&spec, &call).is_err());
    call.call_id = "attempt-shell-0".into();
    call.tool = Some("shell".into());
    call.acp_charge = Some(orbit::acp_contract::Charge::Broker {
        terminal_runtime_seconds: 10,
    });
    assert!(usage.reserve(&spec, &call).is_err()); // permissions cannot be omitted
    call.permissions = spec.permissions.clone();
    assert!(usage.reserve(&spec, &call)?);
    assert!(usage.pending_attempt_call("attempt"));
    let before = serde_json::to_value(&usage)?;
    call.call_id = "attempt-shell-1".into();
    assert!(usage.reserve(&spec, &call).is_err()); // 20 > cumulative 15 seconds
    assert_eq!(before, serde_json::to_value(&usage)?);
    call.acp_charge = Some(orbit::acp_contract::Charge::Broker {
        terminal_runtime_seconds: 5,
    });
    assert!(usage.reserve(&spec, &call)?);
    call.call_id = "attempt-read-0".into();
    call.tool = Some("read_file".into());
    call.permissions = vec!["workspace.read".into()];
    call.acp_charge = Some(orbit::acp_contract::Charge::Broker {
        terminal_runtime_seconds: 0,
    });
    for index in 0..2 {
        call.call_id = format!("attempt-read-{index}");
        assert!(usage.reserve(&spec, &call)?);
    }
    call.call_id = "attempt-read-2".into();
    assert!(usage.reserve(&spec, &call).is_err());
    let restored: Usage = serde_json::from_value(serde_json::to_value(&usage)?)?;
    assert!(restored.tokens.is_none() && restored.cost_microusd.is_none());
    assert_eq!(restored.reservations.len(), 5);
    Ok(())
}
