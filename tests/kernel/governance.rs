use super::*;
use orbit::governance::{Governance, Scope};
use std::os::unix::fs::PermissionsExt;

fn scope(project: &str) -> Scope {
    Scope::parse(&format!("org/{project}/dev")).unwrap()
}
fn credential(f: &Fixture, name: &str) -> Result<Value> {
    let path = f.root.path().join(format!("{name}.credential"));
    std::fs::write(&path, format!("governance-{name}-token-00000000000"))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(json!({"provider":"file","path":path}))
}
fn config(f: &Fixture) -> Result<Config> {
    let policy = json!({"capabilities":["agent.run","human.approval","engine.wait","engine.child"],"repository_ids":[],"agent_bindings":["local-agent"],"max_resources":{"cpu_millis":0,"memory_mib":0,"gpu":0},"max_agent_budget":{"tokens":1000,"cost_microusd":10000,"calls":10},"max_concurrency":8});
    let governance: Governance = serde_json::from_value(json!({
        "organizations":["org"],"projects":[{"organization_id":"org","id":"a","environments":{"dev":policy}},{"organization_id":"org","id":"b","environments":{"dev":policy}}],
        "roles":{"reader":["run.read","artifact.read","definition.read","definition.validate"],"writer":["run.submit","run.signal","run.cancel"],"reviewer":["run.approve"]},
        "principals":{
            "alice":{"kind":"user","credential":credential(f,"alice")?,"grants":[{"scope":scope("a"),"roles":["reader","writer","reviewer"]}]},
            "bob":{"kind":"user","credential":credential(f,"bob")?,"grants":[{"scope":scope("b"),"roles":["reader","writer"]}]},
            "robot":{"kind":"service_account","credential":credential(f,"robot")?,"grants":[{"scope":scope("a"),"roles":["reader","reviewer"]}]}
        },"default_scope":scope("a")
    }))?;
    Ok(Config {
        operator_token: OPERATOR.into(),
        governance: Some(governance),
        agent_bindings: serde_json::from_str(include_str!("../../examples/agent-bindings.json"))?,
        workers: BTreeMap::from([(
            "scoped-agent".into(),
            WorkerIdentity {
                token: CODER.into(),
                capabilities: vec!["agent.run".into(), "agent.fixture-v1".into()],
                scopes: vec![scope("a")],
                ..Default::default()
            },
        )]),
        ..Default::default()
    })
}
fn approval_definition() -> Definition {
    serde_json::from_value(json!({"apiVersion":"orbit/v1","kind":"Definition","metadata":{"name":"governed-approval"},"inputs":{"task":"Review a scoped fixture"},"steps":{"review":{"uses":"human.approval","recovery_policy":"restart_from_inputs","max_attempts":1,"timeout_seconds":60,"retry_backoff_seconds":0,"approval":{"assignees":["alice","robot"],"prompt":"Approve this fixture?"}}}})).unwrap()
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL and loopback HTTP"]
async fn scopes_rbac_service_accounts_policy_and_audit_are_enforced() -> Result<()> {
    let f = Fixture::new().await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(
        axum::serve(
            listener,
            orbit::api::router(App::new(f.engine.clone(), config(&f)?)?),
        )
        .into_future(),
    );
    let alice = Client::new(url.clone(), "governance-alice-token-00000000000".into())?;
    let bob = Client::new(url.clone(), "governance-bob-token-00000000000".into())?;
    let robot = Client::new(url.clone(), "governance-robot-token-00000000000".into())?;
    let operator = Client::new(url, OPERATOR.into())?;
    let body = |project| orbit::api::Submit {
        request_id: id(),
        definition: approval_definition(),
        parent_run_id: None,
        scope: Some(scope(project)),
    };
    let a = alice.post("/runs", &body("a")).await?["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let b = bob.post("/runs", &body("b")).await?["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(alice.get("/runs").await?.as_array().unwrap().len(), 1);
    assert_eq!(bob.get("/runs").await?.as_array().unwrap().len(), 1);
    assert_eq!(operator.get("/runs").await?.as_array().unwrap().len(), 2);
    assert_eq!(alice.get("/projects").await?.as_array().unwrap().len(), 1);
    assert_eq!(alice.get("/projects").await?[0]["scope"], json!(scope("a")));
    assert_eq!(
        alice.get(&format!("/runs/{a}")).await?["submitted_by"],
        "alice"
    );
    assert!(alice.get(&format!("/runs/{b}")).await.is_err());
    assert!(alice.get(&format!("/runs/{b}/events")).await.is_err());
    assert!(
        bob.post(&format!("/runs/{a}/cancel"), &json!({}))
            .await
            .is_err()
    );
    assert!(alice.post("/runs", &body("b")).await.is_err());
    assert!(robot.post("/runs", &body("a")).await.is_err());
    assert!(alice.get("/workers").await.is_err());
    assert!(alice.get("/audit").await.is_err());
    assert_eq!(
        alice.get("/definitions/schema").await?["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    let approval = orbit::agent::Approval {
        request_id: id(),
        step: "review".into(),
        approved: true,
        comment: "scoped review".into(),
    };
    assert!(
        robot
            .post(&format!("/runs/{a}/approvals"), &approval)
            .await
            .is_err()
    );
    assert_eq!(
        alice
            .post(&format!("/runs/{a}/approvals"), &approval)
            .await?["status"],
        "accepted"
    );
    let mut forbidden = body("a");
    forbidden.definition = Definition::parse(include_str!("../../examples/container.yaml"))?;
    assert!(alice.post("/runs", &forbidden).await.is_err());
    f.engine.reconcile().await?;
    let audit = operator.get("/audit").await?;
    let mut previous = String::new();
    for entry in audit.as_array().unwrap() {
        assert_eq!(entry["previous_hash"], previous);
        previous = digest(&serde_json::to_vec(&(&previous, &entry["event"]))?);
        assert_eq!(entry["hash"], previous);
        assert!(!entry.to_string().contains("token-"));
    }
    assert!(
        audit
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["event"]["authorized"] == false)
    );
    assert!(
        f.engine
            .events(&a)
            .await?
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["event"]["actor"] == "alice")
    );
    operator
        .post(&format!("/runs/{b}/cancel"), &json!({}))
        .await?;
    f.engine.reconcile().await?;
    server.abort();
    f.evidence("phase8-scopes-rbac-audit").await?;
    f.control_evidence(
        "phase8-scopes-rbac-audit",
        json!({"audit":f.engine.audit(0).await?}),
    )?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn worker_scopes_and_child_scope_survive_reconnection() -> Result<()> {
    let f = Fixture::new().await?;
    let mut definition = Definition::parse(include_str!("../../examples/agent.yaml"))?;
    definition.steps.retain(|id, _| id == "planner");
    let bindings = serde_json::from_str(include_str!("../../examples/agent-bindings.json"))?;
    let base = Plan::compile_with_agents(definition, RepositoryBinding::none(), &bindings)?;
    for project in ["b", "a"] {
        f.engine
            .submit_as(
                &id(),
                base.clone().in_scope(Some(scope(project)))?,
                None,
                "alice",
            )
            .await?;
    }
    f.engine.reconcile().await?;
    let caps = vec!["agent.run".into(), "agent.fixture-v1".into()];
    let claim = Claim {
        request_id: id(),
        capability: "agent.run".into(),
    };
    assert_eq!(
        f.engine
            .claim_with_capacity("unscoped", &claim, &caps, &Default::default())
            .await?["status"],
        "no_work"
    );
    let a = f
        .engine
        .claim_in_scopes("scoped", &claim, &caps, &Default::default(), &[scope("a")])
        .await?;
    let a: Assignment = serde_json::from_value(a["assignment"].clone())?;
    assert_eq!(a.plan.scope, Some(scope("a")));
    let other = Engine::connect(&f.url, f.engine.artifact_root.clone(), 3).await?;
    assert!(
        other
            .claim_in_scopes(
                "scoped",
                &Claim {
                    request_id: id(),
                    capability: "agent.run".into()
                },
                &caps,
                &Default::default(),
                &[scope("b")]
            )
            .await
            .is_err()
    );
    let mut parent = approval_definition();
    let mut step = parent.steps.remove("review").unwrap();
    step.uses = "engine.child".into();
    step.approval = None;
    step.definition = Some(Box::new(approval_definition()));
    parent.steps.insert("child".into(), step);
    let parent = Plan::compile(parent, RepositoryBinding::none())?.in_scope(Some(scope("a")))?;
    let run = f.engine.submit_as(&id(), parent, None, "alice").await?["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    other.reconcile().await?;
    let snapshot = other.inspect(&run).await?;
    let child = snapshot["tasks"][0]["child_run_ids"][0].as_str().unwrap();
    assert_eq!(other.scope(child).await?, Some(scope("a")));
    assert_eq!(other.inspect(child).await?["submitted_by"], "alice");
    let rows = f.engine.list().await?;
    for row in rows.as_array().unwrap() {
        f.engine.cancel(row["id"].as_str().unwrap()).await?;
    }
    f.engine.reconcile().await?;
    f.evidence("phase8-worker-child-scopes").await?;
    Ok(())
}
