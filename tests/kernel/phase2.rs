use super::*;

fn parent_plan(f: &Fixture, child: Definition, fan: Option<FanOut>) -> Result<Plan> {
    let mut definition = f.plan.definition.clone();
    definition.api_version = "orbit/v1".into();
    definition.steps.clear();
    let mut step: Step = serde_json::from_value(json!({
        "uses": if fan.is_some() { "engine.fan_out" } else { "engine.child" },
        "max_attempts":1,"timeout_seconds":60,"retry_backoff_seconds":0,
        "recovery_policy":"restart_from_inputs","definition":child
    }))?;
    if let Some(source) = fan.as_ref().and_then(|fan| fan.signal_from.as_ref()) {
        step.needs = Some(vec![source.clone()]);
        let wait: Step = serde_json::from_value(
            json!({"uses":"engine.wait","max_attempts":1,"timeout_seconds":60,"retry_backoff_seconds":0,"recovery_policy":"restart_from_inputs"}),
        )?;
        definition.steps.insert(source.clone(), wait);
    }
    step.fan_out = fan;
    definition.steps.insert("dispatch".into(), step);
    definition.steps.insert("done".into(), serde_json::from_value(json!({"uses":"engine.join","needs":["dispatch"],"max_attempts":1,"timeout_seconds":60,"retry_backoff_seconds":0,"recovery_policy":"restart_from_inputs"}))?);
    Plan::compile(definition, f.plan.repository.clone())
}

async fn snapshot(engine: &Engine, run: &str) -> Result<Run> {
    let value: Value = sqlx::query_scalar("SELECT document FROM orbit_runs WHERE id=$1")
        .bind(run)
        .fetch_one(&engine.pool)
        .await?;
    Ok(serde_json::from_value(value)?)
}
fn children(run: &Run) -> Vec<String> {
    run.tasks
        .iter()
        .find(|task| task.step == "dispatch")
        .unwrap()
        .child_run_ids
        .clone()
}
fn literal_fan(items: &[&str], parallel: u32) -> FanOut {
    FanOut {
        max_items: 4,
        max_parallel: parallel,
        items: Some(items.iter().map(|s| s.to_string()).collect()),
        signal_from: None,
        agent_from: None,
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL and HTTP workers"]
async fn dynamic_fan_out_runs_pinned_children_and_joins() -> Result<()> {
    let mut f = Fixture::new().await?;
    f.plan = parent_plan(
        &f,
        f.plan.definition.clone(),
        Some(FanOut {
            max_items: 4,
            max_parallel: 2,
            items: None,
            signal_from: Some("items".into()),
            agent_from: None,
        }),
    )?;
    let root = f.submit().await?;
    f.engine.reconcile().await?;
    assert!(children(&snapshot(&f.engine, &root).await?).is_empty());
    f.engine
        .signal(
            &root,
            &Signal {
                request_id: id(),
                step: "items".into(),
                payload: json!(["first change", "second change", "third change"]),
            },
        )
        .await?;
    f.engine.reconcile().await?;
    let initial = children(&snapshot(&f.engine, &root).await?);
    assert_eq!(initial.len(), 2);
    let restarted = Engine::connect(&f.url, f.engine.artifact_root.clone(), 3).await?;
    restarted.reconcile().await?;
    restarted.reconcile().await?;
    assert_eq!(children(&snapshot(&f.engine, &root).await?), initial);
    let address = address()?;
    let _server = server_process(&f, &address, None).await?;
    for _ in 0..3 {
        for (capability, token) in [("repository.code", CODER), ("repository.test", TESTER)] {
            tokio::time::timeout(
                Duration::from_secs(20),
                worker::run(
                    Client::new(format!("http://{address}"), token.into())?,
                    capability.into(),
                    f.root.path().join("workspaces"),
                    true,
                ),
            )
            .await??;
        }
    }
    wait_state(&f, &root, 0, "SUCCEEDED").await?;
    let result = snapshot(&f.engine, &root).await?;
    assert_eq!(result.state, State::Succeeded);
    assert_eq!(&children(&result)[..2], &initial);
    assert_eq!(children(&result).len(), 3);
    let mut digests = std::collections::BTreeSet::new();
    for (index, child) in children(&result).iter().enumerate() {
        let child = snapshot(&f.engine, child).await?;
        assert_eq!(child.state, State::Succeeded);
        assert_eq!(child.root_run_id.as_deref(), Some(root.as_str()));
        assert_eq!(child.parent_run_id.as_deref(), Some(root.as_str()));
        assert_eq!(
            child.plan.definition.inputs.task,
            ["first change", "second change", "third change"][index]
        );
        assert_eq!(
            child.plan.digest,
            Plan::compile(child.plan.definition.clone(), child.plan.repository.clone())?.digest
        );
        digests.insert(child.plan.digest.clone());
        assert!(!child.tasks[1].accepted_outputs.is_empty());
    }
    assert_eq!(digests.len(), 3);
    f.evidence("phase2-dynamic-fan-out").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn fan_out_empty_and_invalid_inputs_are_bounded() -> Result<()> {
    let mut f = Fixture::new().await?;
    f.plan = parent_plan(
        &f,
        f.plan.definition.clone(),
        Some(FanOut {
            max_items: 2,
            max_parallel: 1,
            items: None,
            signal_from: Some("items".into()),
            agent_from: None,
        }),
    )?;
    for (payload, expected) in [
        (json!([]), State::Succeeded),
        (json!(["a", "b", "c"]), State::Failed),
        (json!([1]), State::Failed),
        (json!({"not":"an array"}), State::Failed),
    ] {
        let root = f.submit().await?;
        f.engine
            .signal(
                &root,
                &Signal {
                    request_id: id(),
                    step: "items".into(),
                    payload,
                },
            )
            .await?;
        f.engine.reconcile().await?;
        let result = snapshot(&f.engine, &root).await?;
        assert_eq!(result.state, expected);
        assert!(children(&result).is_empty());
    }
    f.evidence("phase2-fan-out-bounds").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn nested_child_failure_intervention_and_cancellation_propagate() -> Result<()> {
    for outcome in ["failure", "intervention", "cancel"] {
        let mut f = Fixture::new().await?;
        f.engine
            .set_limits(&Limits {
                max_active_roots: 1,
                ..Limits::default()
            })
            .await?;
        let inner = parent_plan(
            &f,
            f.plan.definition.clone(),
            Some(literal_fan(&["left", "right", "queued"], 2)),
        )?;
        f.plan = parent_plan(&f, inner.definition, None)?;
        let root = f.submit().await?;
        for _ in 0..3 {
            f.engine.reconcile().await?;
        }
        let middle = children(&snapshot(&f.engine, &root).await?)[0].clone();
        let leaves = children(&snapshot(&f.engine, &middle).await?);
        assert_eq!(leaves.len(), 2);
        let a = f.claim("left-worker", "repository.code").await?;
        let b = f.claim("right-worker", "repository.code").await?;
        if outcome == "cancel" {
            f.engine.cancel(&root).await?;
        } else {
            let failure = Failure {
                category: if outcome == "failure" {
                    "task_failure"
                } else {
                    "unknown_failure"
                }
                .into(),
                code: "fixture".into(),
                message: "injected".into(),
                side_effect_status: "none".into(),
            };
            f.engine
                .operate(
                    "left-worker",
                    &operation(
                        &a,
                        Action::Complete {
                            success: false,
                            outputs: vec![],
                            failure: Some(failure),
                        },
                    ),
                )
                .await?;
            assert_eq!(
                snapshot(&f.engine, &root).await?.state,
                if outcome == "failure" {
                    State::Failed
                } else {
                    State::NeedsIntervention
                }
            );
        }
        assert_eq!(
            f.engine
                .claim(
                    "third-worker",
                    &Claim {
                        request_id: id(),
                        capability: "repository.code".into()
                    }
                )
                .await?["status"],
            "no_work"
        );
        if outcome == "failure" {
            // A failed root still occupies admission until its descendants are finalized.
            assert!(
                f.engine
                    .submit(&id(), f.plan.clone(), None)
                    .await
                    .unwrap_err()
                    .to_string()
                    .starts_with("backpressure:")
            );
        }
        if outcome == "intervention" {
            f.engine.cancel(&root).await?;
        }
        assert_eq!(
            f.engine
                .operate("right-worker", &operation(&b, Action::Heartbeat))
                .await?["status"],
            "cancelled"
        );
        assert_eq!(
            snapshot(&f.engine, &middle).await?.state,
            if outcome == "failure" {
                State::Failed
            } else {
                State::CancelRequested
            }
        );
        f.engine.reconcile().await?;
        assert_eq!(children(&snapshot(&f.engine, &middle).await?).len(), 2);
        for child in leaves {
            assert!(snapshot(&f.engine, &child).await?.state.terminal());
        }
        f.evidence(&format!("phase2-tree-{outcome}")).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn child_deadline_cancels_descendants() -> Result<()> {
    let mut f = Fixture::new().await?;
    let child = interaction_plan(&f, false, 60)?.definition;
    f.plan = parent_plan(&f, child, None)?;
    f.plan
        .definition
        .steps
        .get_mut("dispatch")
        .unwrap()
        .timeout_seconds = 1;
    let root = f.submit().await?;
    f.engine.reconcile().await?;
    let child = children(&snapshot(&f.engine, &root).await?)[0].clone();
    f.engine.reconcile().await?;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    f.engine.reconcile().await?;
    assert_eq!(snapshot(&f.engine, &root).await?.state, State::Failed);
    assert_eq!(snapshot(&f.engine, &child).await?.state, State::Cancelled);
    assert!(
        f.engine
            .signal(
                &child,
                &Signal {
                    request_id: id(),
                    step: "resume".into(),
                    payload: json!(null)
                }
            )
            .await
            .is_err()
    );
    f.evidence("phase2-child-deadline").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn shared_admission_and_claim_limits_survive_reconnect() -> Result<()> {
    let mut f = Fixture::new().await?;
    f.plan.definition.api_version = "orbit/v1".into();
    f.plan.definition.max_concurrency = Some(1);
    f.plan
        .definition
        .steps
        .insert("extra".into(), f.plan.definition.steps["code"].clone());
    f.plan = Plan::compile(f.plan.definition.clone(), f.plan.repository.clone())?;
    let limits = Limits {
        max_active_roots: 1,
        max_running_attempts: 2,
        max_attempts_per_worker: 1,
    };
    f.engine.set_limits(&limits).await?;
    let other = Engine::connect(&f.url, f.engine.artifact_root.clone(), 3).await?;
    assert_eq!(other.limits().await?, limits);
    let first_key = id();
    let second_key = id();
    let (first, second) = tokio::join!(
        f.engine.submit(&first_key, f.plan.clone(), None),
        other.submit(&second_key, f.plan.clone(), None)
    );
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    let (accepted, accepted_key, rejected_key) = if let Ok(first) = first {
        (first, first_key, second_key)
    } else {
        (second?, second_key, first_key)
    };
    assert_eq!(
        other.submit(&accepted_key, f.plan.clone(), None).await?,
        accepted
    );
    let root = accepted["run_id"].as_str().unwrap();
    other
        .set_limits(&Limits {
            max_active_roots: 2,
            ..limits.clone()
        })
        .await?;
    let second_root = other.submit(&rejected_key, f.plan.clone(), None).await?;
    f.engine.reconcile().await?;
    let a = Claim {
        request_id: id(),
        capability: "repository.code".into(),
    };
    let b = Claim {
        request_id: id(),
        capability: "repository.code".into(),
    };
    let (a, b) = tokio::join!(
        f.engine.claim("shared-worker", &a),
        other.claim("shared-worker", &b)
    );
    let (a, b) = (a?, b?);
    assert_eq!(
        usize::from(a["status"] == "accepted") + usize::from(b["status"] == "accepted"),
        1
    );
    let assignment: Assignment = serde_json::from_value(if a["status"] == "accepted" {
        a["assignment"].clone()
    } else {
        b["assignment"].clone()
    })?;
    let another = other
        .claim(
            "different-worker",
            &Claim {
                request_id: id(),
                capability: "repository.code".into(),
            },
        )
        .await?;
    assert_eq!(another["status"], "accepted");
    assert_ne!(another["assignment"]["run_id"], assignment.run_id);
    assert_eq!(
        f.engine
            .claim(
                "third-worker",
                &Claim {
                    request_id: id(),
                    capability: "repository.code".into()
                }
            )
            .await?["status"],
        "no_work"
    );
    other
        .set_limits(&Limits {
            max_active_roots: 2,
            max_running_attempts: 8,
            max_attempts_per_worker: 8,
        })
        .await?;
    // Per-run limits still block both extra tasks even after raising global/worker limits.
    assert_eq!(
        f.engine
            .claim(
                "third-worker",
                &Claim {
                    request_id: id(),
                    capability: "repository.code".into()
                }
            )
            .await?["status"],
        "no_work"
    );
    f.expire(&assignment).await?;
    f.engine.reconcile().await?;
    assert_eq!(
        other
            .claim(
                "shared-worker",
                &Claim {
                    request_id: id(),
                    capability: "repository.code".into()
                }
            )
            .await?["status"],
        "accepted"
    );
    f.engine.cancel(root).await?;
    f.engine
        .cancel(second_root["run_id"].as_str().unwrap())
        .await?;
    f.engine.reconcile().await?;
    f.evidence("phase2-shared-limits").await?;
    Ok(())
}

#[cfg(feature = "fault-injection")]
#[tokio::test]
#[ignore = "requires PostgreSQL and process fault injection"]
async fn child_creation_survives_commit_boundary_kills() -> Result<()> {
    for point in ["children_before_commit", "children_after_commit"] {
        let mut f = Fixture::new().await?;
        let child = interaction_plan(&f, false, 60)?.definition;
        f.plan = parent_plan(
            &f,
            child,
            Some(literal_fan(&["first", "second", "queued"], 2)),
        )?;
        let root = f.submit().await?;
        let address = address()?;
        let server = server_process(&f, &address, Some(point)).await?;
        wait_file(&f.root.path().join("fault.marker")).await?;
        drop(server);
        let before = snapshot(&f.engine, &root).await?;
        assert_eq!(
            children(&before).len(),
            if point == "children_before_commit" {
                0
            } else {
                2
            }
        );
        let _server = server_process(&f, &address, None).await?;
        f.engine.reconcile().await?;
        let first = children(&snapshot(&f.engine, &root).await?);
        f.engine.reconcile().await?;
        f.engine.reconcile().await?;
        assert_eq!(first.len(), 2);
        assert_eq!(children(&snapshot(&f.engine, &root).await?), first);
        if point == "children_after_commit" {
            assert_eq!(children(&before), first);
        }
        let actual: i64 =
            sqlx::query_scalar("SELECT count(*) FROM orbit_runs WHERE document->>'root_run_id'=$1")
                .bind(&root)
                .fetch_one(&f.engine.pool)
                .await?;
        assert_eq!(actual, 2);
        f.engine.cancel(&root).await?;
        f.engine.reconcile().await?;
        f.evidence(point).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL and local server processes"]
async fn limits_cli_and_two_servers_enforce_backpressure() -> Result<()> {
    let mut f = Fixture::new().await?;
    f.plan.definition.api_version = "orbit/v1".into();
    f.plan
        .definition
        .steps
        .insert("extra".into(), f.plan.definition.steps["code"].clone());
    f.plan = Plan::compile(f.plan.definition.clone(), f.plan.repository.clone())?;
    let first_address = address()?;
    let second_address = address()?;
    let _first = server_process(&f, &first_address, None).await?;
    let _second = server_process(&f, &second_address, None).await?;
    let first_url = format!("http://{first_address}");
    let second_url = format!("http://{second_address}");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_orbit"))
        .args([
            "--url",
            &first_url,
            "set-limits",
            "--max-active-roots",
            "1",
            "--max-running-attempts",
            "1",
            "--max-attempts-per-worker",
            "8",
        ])
        .env("ORBIT_TOKEN", OPERATOR)
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let operator = Client::new(second_url.clone(), OPERATOR.into())?;
    assert_eq!(operator.get("/limits").await?["max_running_attempts"], 1);
    assert!(
        Client::new(first_url.clone(), CODER.into())?
            .post("/limits", &Limits::default())
            .await
            .is_err()
    );
    let root = f.submit().await?;
    f.engine.reconcile().await?;
    let request_id = id();
    let rejected = reqwest::Client::new()
        .post(format!("{second_url}/runs"))
        .bearer_auth(OPERATOR)
        .json(&orbit::api::Submit {
            scope: None,
            request_id: request_id.clone(),
            definition: f.plan.definition.clone(),
            parent_run_id: None,
        })
        .send()
        .await?;
    assert_eq!(rejected.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(rejected.headers()["retry-after"], "1");
    let coder1 = Client::new(first_url, CODER.into())?;
    let coder2 = Client::new(second_url, CODER.into())?;
    let first_claim = Claim {
        request_id: id(),
        capability: "repository.code".into(),
    };
    let second_claim = Claim {
        request_id: id(),
        capability: "repository.code".into(),
    };
    let (a, b) = tokio::join!(
        coder1.post("/worker/claim", &first_claim),
        coder2.post("/worker/claim", &second_claim)
    );
    let (a, b) = (a?, b?);
    assert_eq!(
        usize::from(a["status"] == "accepted") + usize::from(b["status"] == "accepted"),
        1
    );
    f.engine.cancel(&root).await?;
    f.engine.reconcile().await?;
    let accepted = f.engine.submit(&request_id, f.plan.clone(), None).await?;
    assert_eq!(accepted["status"], "accepted");
    f.engine
        .cancel(accepted["run_id"].as_str().unwrap())
        .await?;
    f.engine.reconcile().await?;
    f.evidence("phase2-limits-two-servers").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn concurrent_bootstrap_initializes_shared_limits_once() -> Result<()> {
    let f = Fixture::new().await?;
    let base = std::env::var("ORBIT_TEST_DATABASE_URL")?;
    let schema = format!("orbit_bootstrap_{}", id().replace('-', ""));
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&f.engine.pool)
        .await?;
    let separator = if base.contains('?') { '&' } else { '?' };
    let url = format!("{base}{separator}options=-csearch_path%3D{schema}");
    let (first, second) = tokio::join!(
        Engine::connect(&url, f.root.path().join("bootstrap-artifacts"), 3),
        Engine::connect(&url, f.root.path().join("bootstrap-artifacts"), 3)
    );
    let (first, second) = (first?, second?);
    assert_eq!(first.limits().await?, Limits::default());
    let changed = Limits {
        max_active_roots: 2,
        ..Limits::default()
    };
    first.set_limits(&changed).await?;
    assert_eq!(second.limits().await?, changed);
    let third = Engine::connect(&url, f.root.path().join("bootstrap-artifacts"), 3).await?;
    assert_eq!(third.limits().await?, changed);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM orbit_control")
        .fetch_one(&third.pool)
        .await?;
    assert_eq!(rows, 1);
    // Retain a normal run snapshot in this schema alongside its limit-change audit.
    let run = first.submit(&id(), f.plan.clone(), None).await?;
    first.cancel(run["run_id"].as_str().unwrap()).await?;
    first.reconcile().await?;
    Fixture {
        engine: first,
        url,
        root: f.root,
        plan: f.plan,
    }
    .evidence("phase2-concurrent-bootstrap")
    .await?;
    Ok(())
}
