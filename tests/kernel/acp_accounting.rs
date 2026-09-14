use super::*;
use orbit::agent::{Binding, CallReceipt, CallReservation};

async fn assignment(f: &Fixture) -> Result<(String, Assignment)> {
    let raw: Value = serde_json::from_str(include_str!("../fixtures/acp-contract.json"))?;
    let binding: Binding = serde_json::from_value(raw["binding"].clone())?;
    let mut definition = f.plan.definition.clone();
    definition.api_version = "orbit/v1".into();
    definition.steps.retain(|name, _| name == "code");
    let code = definition.steps.get_mut("code").unwrap();
    code.agent = Some(serde_json::from_value(raw["agent"].clone())?);
    code.resources = Some(orbit::compute::Resources {
        cpu_millis: 1000,
        memory_mib: 128,
        gpu: 0,
    });
    code.execution = Some(serde_json::from_value(
        json!({"isolation":"trusted","network":"none","filesystem":"workspace"}),
    )?);
    let profile = serde_json::from_value(
        json!({"backend":"rootless_podman","image":format!("fixture@sha256:{}", "a".repeat(64))}),
    )?;
    let plan = Plan::compile_with_execution(
        definition,
        f.plan.repository.clone(),
        &BTreeMap::from([("codex-fixture".into(), binding.clone())]),
        &BTreeMap::from([(orbit::execution::Isolation::Trusted, profile)]),
    )?;
    let run = f.engine.submit(&id(), plan, None).await?["run_id"]
        .as_str()
        .unwrap()
        .to_owned();
    f.engine.reconcile().await?;
    let capabilities = vec![
        "repository.code".into(),
        orbit::execution::CAPABILITY.into(),
        binding.runtime,
    ];
    let capacity = orbit::compute::WorkerCapacity {
        pool: None,
        resources: orbit::compute::Resources {
            cpu_millis: 1000,
            memory_mib: 128,
            gpu: 0,
        },
    };
    let claimed = f
        .engine
        .claim_with_capacity(
            "acp-coder",
            &Claim {
                request_id: id(),
                capability: "repository.code".into(),
            },
            &capabilities,
            &capacity,
        )
        .await?;
    let a: Assignment = serde_json::from_value(claimed["assignment"].clone())?;
    f.start("acp-coder", &a).await?;
    Ok((run, a))
}

fn prompt(a: &Assignment) -> CallReservation {
    serde_json::from_value(
        json!({"call_id":format!("{}-prompt-0",a.attempt_id), "tool":null,"permissions":[],
        "request_digest":"b".repeat(64),"acp_charge":{"kind":"prompt"}}),
    )
    .unwrap()
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn acp_accounting_competing_servers_replay_and_cancel_fencing() -> Result<()> {
    let f = Fixture::new().await?;
    let (run, a) = assignment(&f).await?;
    let other = Engine::connect(&f.url, f.engine.artifact_root.clone(), 3).await?;
    let reservation = prompt(&a);
    let operation1 = operation(
        &a,
        Action::ReserveAgentCall {
            reservation: reservation.clone(),
        },
    );
    assert_eq!(
        f.engine.operate("acp-coder", &operation1).await?["replayed"],
        false
    );
    let mut replay = operation1.clone();
    replay.request_id = id();
    assert_eq!(other.operate("acp-coder", &replay).await?["replayed"], true);
    let mut batch = orbit::acp_contract::RecordBatch {
        attempt_id: a.attempt_id.clone(),
        session_digest: "e".repeat(64),
        sequence: 0,
        records: vec![orbit::acp_contract::Record {
            kind: orbit::acp_contract::RecordKind::Started,
            digest: "f".repeat(64),
            output_bytes: 0,
            reported_tool_calls: 0,
        }],
    };
    let record = operation(
        &a,
        Action::RecordAcpSession {
            batch: batch.clone(),
        },
    );
    assert_eq!(
        f.engine.operate("acp-coder", &record).await?["replayed"],
        false
    );
    assert_eq!(
        other
            .operate(
                "acp-coder",
                &operation(
                    &a,
                    Action::RecordAcpSession {
                        batch: batch.clone()
                    }
                )
            )
            .await?["replayed"],
        true
    );
    batch.sequence = 2;
    batch.records[0].kind = orbit::acp_contract::RecordKind::Update;
    assert!(
        other
            .operate(
                "acp-coder",
                &operation(
                    &a,
                    Action::RecordAcpSession {
                        batch: batch.clone()
                    }
                )
            )
            .await
            .is_err()
    );
    batch.sequence = 1;
    let make_shell = |suffix| {
        operation(&a,Action::ReserveAgentCall {reservation:serde_json::from_value(json!({
        "call_id":format!("{}-shell-{suffix}",a.attempt_id),"tool":"shell", "permissions":["workspace.read","workspace.write","shell.execute"],
        "request_digest":"c".repeat(64),"acp_charge":{"kind":"broker","terminal_runtime_seconds":10}})).unwrap()})
    };
    let first = make_shell(0);
    let second = make_shell(1);
    let (x, y) = tokio::join!(
        f.engine.operate("acp-coder", &first),
        other.operate("acp-coder", &second)
    );
    assert_eq!(usize::from(x.is_ok()) + usize::from(y.is_ok()), 1);
    let state = other.inspect(&run).await?;
    assert_eq!(state["tasks"][0]["agent_usage"]["tokens"], Value::Null);
    assert_eq!(
        state["tasks"][0]["agent_usage"]["cost_microusd"],
        Value::Null
    );
    assert_eq!(
        state["tasks"][0]["agent_usage"]["reservations"]
            .as_object()
            .unwrap()
            .len(),
        2
    );
    f.engine.cancel(&run).await?;
    assert_eq!(
        other
            .operate(
                "acp-coder",
                &operation(&a, Action::RecordAcpSession { batch })
            )
            .await?["status"],
        "cancelled"
    );
    let finish = operation(
        &a,
        Action::FinishAgentCall {
            receipt: CallReceipt {
                call_id: reservation.call_id,
                attempt_id: a.attempt_id.clone(),
                result_digest: "d".repeat(64),
                external_id: None,
            },
        },
    );
    assert_eq!(
        other.operate("acp-coder", &finish).await?["status"],
        "cancelled"
    );
    assert_eq!(
        other.operate("acp-coder", &make_shell(2)).await?["status"],
        "cancelled"
    );
    assert_eq!(
        other.inspect(&run).await?["tasks"][0]["agent_usage"],
        state["tasks"][0]["agent_usage"]
    );
    other.reconcile().await?;
    assert_eq!(other.inspect(&run).await?["state"], "CANCELLED");
    assert_eq!(
        other.inspect(&run).await?["tasks"][0]["agent_usage"],
        state["tasks"][0]["agent_usage"]
    );
    f.evidence("acp-accounting-transactional").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn acp_accounting_unconfirmed_prompt_blocks_automatic_retry() -> Result<()> {
    let mut f = Fixture::new().await?;
    f.engine.lease_seconds = 2;
    let (run, a) = assignment(&f).await?;
    f.engine
        .operate(
            "acp-coder",
            &operation(
                &a,
                Action::ReserveAgentCall {
                    reservation: prompt(&a),
                },
            ),
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(2100)).await;
    f.engine.reconcile().await?;
    let state = f.engine.inspect(&run).await?;
    assert_eq!(state["tasks"][0]["state"], "NEEDS_INTERVENTION");
    assert_eq!(state["tasks"][0]["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(
        state["tasks"][0]["agent_usage"]["reservations"]
            .as_object()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        f.engine
            .operate(
                "acp-coder",
                &operation(
                    &a,
                    Action::ReserveAgentCall {
                        reservation: prompt(&a)
                    }
                )
            )
            .await?["status"],
        "ownership_lost"
    );
    f.evidence("acp-accounting-uncertain-prompt").await?;
    Ok(())
}
