use super::*;
use orbit::agent::{AgentReport, Approval, Binding, CallReservation};

fn bindings() -> BTreeMap<String, Binding> {
    serde_json::from_str(include_str!("../../examples/agent-bindings.json")).unwrap()
}
fn definition() -> Definition {
    Definition::parse(include_str!("../../examples/agent.yaml")).unwrap()
}
fn plan(definition: Definition) -> Result<Plan> {
    Plan::compile_with_agents(definition, RepositoryBinding::none(), &bindings())
}
async fn claim(engine: &Engine) -> Result<Assignment> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            engine.reconcile().await?;
            let result = engine
                .claim_with_capacity(
                    "agent",
                    &Claim {
                        request_id: id(),
                        capability: "agent.run".into(),
                    },
                    &["agent.run".into(), "agent.fixture-v1".into()],
                    &Default::default(),
                )
                .await?;
            if result["status"] == "accepted" {
                return Ok(serde_json::from_value(result["assignment"].clone())?);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?
}
async fn report(f: &Fixture, a: &Assignment, items: Vec<String>) -> Result<()> {
    f.start("agent", a).await?;
    let report = AgentReport {
        attempt_id: a.attempt_id.clone(),
        binding_digest: a.agent_binding_digest.clone().unwrap(),
        output: json!({"result":"fixture"}),
        delegation_inputs: items,
    };
    let logs = f.upload("agent", a, "logs", b"fixture output").await?;
    let report = f
        .upload("agent", a, "agent_report", &serde_json::to_vec(&report)?)
        .await?;
    assert_eq!(
        f.engine
            .operate(
                "agent",
                &operation(
                    a,
                    Action::Complete {
                        success: true,
                        outputs: vec![logs, report],
                        failure: None
                    }
                )
            )
            .await?["status"],
        "accepted"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn agent_execution_lifecycle_is_durable_before_report_and_replay_safe() -> Result<()> {
    let f = Fixture::new().await?;
    let mut def = definition();
    def.steps.retain(|name, _| name == "planner");
    let run = f.engine.submit(&id(), plan(def)?, None).await?["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let a = claim(&f.engine).await?;
    f.start("agent", &a).await?;

    let start = operation(
        &a,
        Action::StartExecution {
            evidence: orbit::continuation::AgentExecutionStart {
                agent_type: "fixture-agent".into(),
                requested_model: Some("requested-model".into()),
                resolved_model: Some("resolved-model".into()),
                runtime_image: Some("fixture-image".into()),
                runtime_digest: Some("fixture-digest".into()),
                credential_reference: Some("fixture-credential".into()),
                ..Default::default()
            },
        },
    );
    let accepted = f.engine.operate("agent", &start).await?;
    assert_eq!(accepted["replayed"], false);
    let execution_id = accepted["execution_id"].as_str().unwrap().to_string();
    let pending = f.engine.inspect(&run).await?;
    let execution = &pending["tasks"][0]["attempts"][0]["agent_executions"][0];
    assert_eq!(execution["execution_id"], execution_id);
    assert_eq!(execution["status"], "pending");
    assert_eq!(execution["actual_model"], Value::Null);

    let mut duplicate_start = start.clone();
    duplicate_start.request_id = id();
    let replayed = f.engine.operate("agent", &duplicate_start).await?;
    assert_eq!(replayed["replayed"], true);
    assert_eq!(replayed["execution_id"], execution_id);

    f.engine
        .operate(
            "agent",
            &operation(
                &a,
                Action::MarkExecutionRunning {
                    execution_id: execution_id.clone(),
                },
            ),
        )
        .await?;
    f.engine
        .operate(
            "agent",
            &operation(
                &a,
                Action::UpdateExecution {
                    execution_id: execution_id.clone(),
                    actual_model: Some("confirmed-model".into()),
                    turn_count: Some(1),
                    tool_call_count: Some(17),
                    tool_success_count: Some(16),
                    tool_failure_count: Some(1),
                    tool_counts: BTreeMap::from([("shell".into(), 17)]),
                },
            ),
        )
        .await?;
    let running = f.engine.inspect(&run).await?;
    assert_eq!(
        running["tasks"][0]["attempts"][0]["agent_executions"][0]["status"],
        "running"
    );
    assert_eq!(
        running["tasks"][0]["attempts"][0]["agent_executions"][0]["actual_model"],
        "confirmed-model"
    );
    assert_eq!(
        running["tasks"][0]["attempts"][0]["agent_executions"][0]["tool_call_count"],
        17
    );

    let logs = f.upload("agent", &a, "logs", b"fixture output").await?;
    let report = AgentReport {
        attempt_id: a.attempt_id.clone(),
        binding_digest: a.agent_binding_digest.clone().unwrap(),
        output: json!({"result":"fixture"}),
        delegation_inputs: vec![],
    };
    let report = f
        .upload("agent", &a, "agent_report", &serde_json::to_vec(&report)?)
        .await?;
    let complete = operation(
        &a,
        Action::Complete {
            success: true,
            outputs: vec![logs, report],
            failure: None,
        },
    );
    f.engine.operate("agent", &complete).await?;
    assert_eq!(
        f.engine.operate("agent", &complete).await?["status"],
        "accepted"
    );
    let final_state = f.engine.inspect(&run).await?;
    let executions = final_state["tasks"][0]["attempts"][0]["agent_executions"]
        .as_array()
        .unwrap();
    assert_eq!(executions.len(), 1);
    assert_eq!(executions[0]["execution_id"], execution_id);
    assert_eq!(executions[0]["status"], "completed");
    assert_eq!(executions[0]["actual_model"], "confirmed-model");
    assert_eq!(executions[0]["usage"], Value::Null);
    assert_eq!(
        final_state["tasks"][0]["attempts"][0]["agent_executions"][0]["metadata"]["credential_reference"],
        "fixture-credential"
    );

    let reopened = Engine::connect(&f.url, f.engine.artifact_root.clone(), 3).await?;
    assert_eq!(
        reopened.inspect(&run).await?["tasks"][0]["attempts"][0]["agent_executions"][0]["execution_id"],
        execution_id
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn agent_execution_failure_and_budget_exhaustion_finalize_existing_records() -> Result<()> {
    let f = Fixture::new().await?;
    let mut def = definition();
    def.steps.retain(|name, _| name == "planner");
    let plan = plan(def)?;
    let run = f.engine.submit(&id(), plan.clone(), None).await?["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let a = claim(&f.engine).await?;
    f.start("agent", &a).await?;
    let start = |a: &Assignment| {
        operation(
            a,
            Action::StartExecution {
                evidence: orbit::continuation::AgentExecutionStart {
                    agent_type: "fixture-agent".into(),
                    ..Default::default()
                },
            },
        )
    };
    let first = f.engine.operate("agent", &start(&a)).await?;
    let first_id = first["execution_id"].as_str().unwrap().to_string();
    f.engine
        .operate(
            "agent",
            &operation(
                &a,
                Action::Complete {
                    success: false,
                    outputs: vec![],
                    failure: Some(Failure {
                        category: "infrastructure_failure".into(),
                        code: "coding_agent_failed".into(),
                        message: "runtime failed before report".into(),
                        side_effect_status: "none".into(),
                    }),
                },
            ),
        )
        .await?;
    let failed = f.engine.inspect(&run).await?;
    assert_eq!(
        failed["tasks"][0]["attempts"][0]["agent_executions"][0]["execution_id"],
        first_id
    );
    assert_eq!(
        failed["tasks"][0]["attempts"][0]["agent_executions"][0]["status"],
        "failed"
    );
    assert_eq!(
        failed["tasks"][0]["attempts"][0]["agent_executions"][0]["termination_reason"],
        "infrastructure_error"
    );

    let run = f.engine.submit(&id(), plan, None).await?["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let a = claim(&f.engine).await?;
    f.start("agent", &a).await?;
    let execution_id = f.engine.operate("agent", &start(&a)).await?["execution_id"]
        .as_str()
        .unwrap()
        .to_string();
    f.engine
        .operate(
            "agent",
            &operation(
                &a,
                Action::Complete {
                    success: false,
                    outputs: vec![],
                    failure: Some(Failure {
                        category: "task_failure".into(),
                        code: "budget_exhausted".into(),
                        message: "budget".into(),
                        side_effect_status: "none".into(),
                    }),
                },
            ),
        )
        .await?;
    let exhausted = f.engine.inspect(&run).await?;
    assert_eq!(
        exhausted["tasks"][0]["attempts"][0]["agent_executions"][0]["execution_id"],
        execution_id
    );
    assert_eq!(
        exhausted["tasks"][0]["attempts"][0]["agent_executions"][0]["status"],
        "interrupted"
    );
    assert_eq!(
        exhausted["tasks"][0]["attempts"][0]["agent_executions"][0]["termination_reason"],
        "budget_exhausted"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn agent_budgets_permissions_and_retries_are_transactional() -> Result<()> {
    let f = Fixture::new().await?;
    let mut def = definition();
    def.steps.retain(|name, _| name == "planner");
    let run = f.engine.submit(&id(), plan(def)?, None).await?["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    f.engine.reconcile().await?;
    assert_eq!(
        f.engine
            .claim(
                "unbound",
                &Claim {
                    request_id: id(),
                    capability: "agent.run".into()
                }
            )
            .await?["status"],
        "no_work"
    );
    let a = claim(&f.engine).await?;
    f.start("agent", &a).await?;
    let reservation = |key: &str, tokens| Action::ReserveAgentCall {
        reservation: CallReservation {
            request_digest: None,
            acp_charge: None,
            call_id: key.into(),
            tokens: Some(tokens),
            cost_microusd: Some(1),
            tool: None,
            permissions: vec![],
        },
    };
    let first = operation(&a, reservation("call-a", 600));
    let second = operation(&a, reservation("call-b", 600));
    let other = Engine::connect(&f.url, f.engine.artifact_root.clone(), 3).await?;
    let (x, y) = tokio::join!(
        f.engine.operate("agent", &first),
        other.operate("agent", &second)
    );
    assert_eq!(usize::from(x.is_ok()) + usize::from(y.is_ok()), 1);
    let accepted = if x.is_ok() { &first } else { &second };
    assert_eq!(other.operate("agent", accepted).await?["replayed"], false);
    let mut duplicate = accepted.clone();
    duplicate.request_id = id();
    assert_eq!(other.operate("agent", &duplicate).await?["replayed"], true);
    let forbidden = operation(
        &a,
        Action::ReserveAgentCall {
            reservation: CallReservation {
                request_digest: None,
                acp_charge: None,
                call_id: "forbidden".into(),
                tokens: Some(1),
                cost_microusd: Some(0),
                tool: Some("shell".into()),
                permissions: vec![],
            },
        },
    );
    assert!(other.operate("agent", &forbidden).await.is_err());
    f.engine
        .operate(
            "agent",
            &operation(
                &a,
                Action::Complete {
                    success: false,
                    outputs: vec![],
                    failure: Some(Failure {
                        category: "infrastructure_failure".into(),
                        code: "runtime_unavailable".into(),
                        message: "retry".into(),
                        side_effect_status: "none".into(),
                    }),
                },
            ),
        )
        .await?;
    let b = claim(&other).await?;
    assert_eq!(b.generation, 2);
    f.start("agent", &b).await?;
    assert!(
        other
            .operate("agent", &operation(&b, reservation("too-much", 401)))
            .await
            .is_err()
    );
    assert_eq!(
        other
            .operate("agent", &operation(&a, reservation("stale", 1)))
            .await?["status"],
        "ownership_lost"
    );
    assert_eq!(
        other
            .operate("agent", &operation(&b, reservation("remaining", 400)))
            .await?["tokens_reserved"],
        1000
    );
    assert_eq!(
        other.get_attempt("agent", &run, &b.attempt_id).await?["agent_usage"]["tokens"],
        1000
    );
    f.engine.cancel(&run).await?;
    assert_eq!(
        other
            .operate("agent", &operation(&b, reservation("cancelled", 0)))
            .await?["status"],
        "cancelled"
    );
    f.engine.reconcile().await?;
    f.evidence("phase5-agent-budget").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn agent_delegation_waits_for_assigned_approval_and_pins_children() -> Result<()> {
    let f = Fixture::new().await?;
    let run = f.engine.submit(&id(), plan(definition())?, None).await?["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let a = claim(&f.engine).await?;
    report(
        &f,
        &a,
        vec!["first work item".into(), "second work item".into()],
    )
    .await?;
    f.engine.reconcile().await?;
    let state = f.engine.inspect(&run).await?;
    assert_eq!(state["tasks"][0]["child_run_ids"], Value::Null);
    let approval = Approval {
        request_id: id(),
        step: "review".into(),
        approved: true,
        comment: "reviewed fixture result".into(),
    };
    assert!(f.engine.approve(&run, &approval, "outsider").await.is_err());
    assert!(
        f.engine
            .signal(
                &run,
                &Signal {
                    request_id: id(),
                    step: "review".into(),
                    payload: json!({"approved":true})
                }
            )
            .await
            .is_err()
    );
    let accepted = f.engine.approve(&run, &approval, "operator").await?;
    let other = Engine::connect(&f.url, f.engine.artifact_root.clone(), 3).await?;
    assert_eq!(accepted, other.approve(&run, &approval, "operator").await?);
    for expected in ["first work item", "second work item"] {
        let child = claim(&other).await?;
        assert_eq!(child.plan.definition.inputs.task, expected);
        assert_eq!(child.agent_binding_digest, a.agent_binding_digest);
        report(&f, &child, vec![]).await?;
        other.reconcile().await?;
    }
    assert_eq!(f.engine.inspect(&run).await?["state"], "SUCCEEDED");
    let journals = f.engine.events(&run).await?;
    assert_eq!(
        journals
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["event"]["type"] == "APPROVAL_RECEIVED")
            .count(),
        1
    );
    f.evidence("phase5-agent-delegation").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL and HTTP"]
async fn approval_authorization_early_denial_deadline_and_cancellation() -> Result<()> {
    let f = Fixture::new().await?;
    let mut def = definition();
    def.steps.retain(|name, _| name == "review");
    let review = def.steps.get_mut("review").unwrap();
    review.needs = None;
    review.timeout_seconds = 1;
    review.approval.as_mut().unwrap().assignees = vec!["reviewer".into()];
    let config = Config {
        operator_token: OPERATOR.into(),
        approvers: BTreeMap::from([("reviewer".into(), "reviewer-test-token-00000000000".into())]),
        ..Default::default()
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(
        axum::serve(
            listener,
            orbit::api::router(App::new(f.engine.clone(), config)?),
        )
        .into_future(),
    );
    let reviewer = Client::new(url.clone(), "reviewer-test-token-00000000000".into())?;
    assert!(reviewer.get("/runs").await.is_err());
    for scenario in ["deny", "expire", "cancel"] {
        let run = f.engine.submit(&id(), plan(def.clone())?, None).await?["run_id"]
            .as_str()
            .unwrap()
            .to_string();
        let body = Approval {
            request_id: id(),
            step: "review".into(),
            approved: false,
            comment: "fixture decision".into(),
        };
        let path = format!("/runs/{run}/approvals");
        assert!(
            Client::new(url.clone(), OPERATOR.into())?
                .post(&path, &body)
                .await
                .is_err()
        );
        match scenario {
            "deny" => {
                assert_eq!(reviewer.post(&path, &body).await?["status"], "accepted");
                f.engine.reconcile().await?;
                assert_eq!(f.engine.inspect(&run).await?["state"], "FAILED");
            }
            "expire" => {
                f.engine.reconcile().await?;
                tokio::time::sleep(Duration::from_millis(1100)).await;
                assert!(reviewer.post(&path, &body).await.is_err());
                f.engine.reconcile().await?;
                assert_eq!(f.engine.inspect(&run).await?["state"], "FAILED");
            }
            _ => {
                f.engine.cancel(&run).await?;
                assert!(reviewer.post(&path, &body).await.is_err());
                f.engine.reconcile().await?;
            }
        }
    }
    server.abort();
    f.evidence("phase5-human-approval").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL and actual server/Python runtime processes"]
async fn agent_runtime_survives_server_worker_kills_without_resetting_budget() -> Result<()> {
    let f = Fixture::new().await?;
    let config = Config {
        operator_token: OPERATOR.into(),
        agent_bindings: bindings(),
        workers: BTreeMap::from([(
            "agent".into(),
            WorkerIdentity {
                token: CODER.into(),
                capabilities: vec!["agent.run".into(), "agent.fixture-v1".into()],
                ..Default::default()
            },
        )]),
        ..Default::default()
    };
    let config_path = f.root.path().join("agent-server.json");
    std::fs::write(&config_path, serde_json::to_vec(&config)?)?;
    let address = address()?;
    let mut server = server_process_configured(&f, &address, None, config_path.clone()).await?;
    let mut def = definition();
    def.steps.retain(|name, _| name == "planner");
    let operator = Client::new(format!("http://{address}"), OPERATOR.into())?;
    let accepted = operator
        .post(
            "/runs",
            &orbit::api::Submit {
                scope: None,
                request_id: id(),
                definition: def,
                parent_run_id: None,
            },
        )
        .await?;
    let run = accepted["run_id"].as_str().unwrap();
    let runtime_root = f.root.path().join("agent-fixture");
    let spawn = || -> Result<ChildGuard> {
        Ok(ChildGuard(
            std::process::Command::new("python3")
                .arg(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/agent-runtime.py"
                ))
                .env(
                    "PYTHONPATH",
                    concat!(env!("CARGO_MANIFEST_DIR"), "/sdk/python"),
                )
                .env("ORBIT_URL", format!("http://{address}"))
                .env("ORBIT_TOKEN", CODER)
                .env("ORBIT_AGENT_FIXTURE", &runtime_root)
                .env("ORBIT_AGENT_KILL_TEST", "1")
                .stderr(
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(f.root.path().join("agent-runtime.log"))?,
                )
                .spawn()?,
        ))
    };
    let mut worker = spawn()?;
    wait_file(&runtime_root.join("reserved.json")).await?;
    let first: Value =
        serde_json::from_slice(&tokio::fs::read(runtime_root.join("reserved.json")).await?)?;
    assert_eq!(first["generation"], 1);
    worker.0.kill()?;
    worker.0.wait()?;
    server.0.kill()?;
    server.0.wait()?;
    let _server = server_process_configured(&f, &address, None, config_path).await?;
    let mut replacement = spawn()?;
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            if operator.get(&format!("/runs/{run}")).await?["state"] == "SUCCEEDED" {
                break;
            }
            if let Some(status) = replacement.0.try_wait()? {
                anyhow::ensure!(status.success(), "agent runtime failed: {status}");
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    let state = operator.get(&format!("/runs/{run}")).await?;
    assert_eq!(state["tasks"][0]["attempts"].as_array().unwrap().len(), 2);
    assert_eq!(state["tasks"][0]["agent_usage"]["tokens"], 200);
    assert_ne!(
        state["tasks"][0]["attempts"][1]["workspace_id"],
        first["workspace_id"]
    );
    assert_eq!(state["tasks"][0]["attempts"][0]["state"], "LOST");
    let result = orbit::mcp::call(&operator, "get_run", &json!({"run_id":run})).await?;
    assert_eq!(result, state);
    f.evidence("phase5-agent-process-recovery").await?;
    Ok(())
}
