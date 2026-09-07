use orbit::model::*;

fn definition() -> Definition {
    serde_json::from_value(serde_json::json!({
        "apiVersion":"orbit/v0","kind":"Definition","metadata":{"name":"implement"},
        "inputs":{"repository_id":"fixture","base_revision":"a".repeat(40),"task":"Fix addition"},
        "steps":{
            "code":{"uses":"repository.code","max_attempts":3,"timeout_seconds":60,"retry_backoff_seconds":0,"recovery_policy":"restart_from_inputs"},
            "test":{"uses":"repository.test","needs":["code"],"max_attempts":2,"timeout_seconds":60,"retry_backoff_seconds":0,"recovery_policy":"restart_from_inputs","commands":[{"argv":["sh","test.sh"],"cwd":".","timeout_seconds":10}]}
        }
    })).unwrap()
}
#[test]
fn strict_definition_and_immutable_plan() {
    let definition = definition();
    definition.validate().unwrap();
    let repository = RepositoryBinding {
        path: "/tmp/fixture".into(),
        coding_command: CommandSpec {
            argv: vec!["agent".into()],
            cwd: ".".into(),
            timeout_seconds: 60,
        },
        allowed_test_executables: vec!["sh".into()],
    };
    let plan = Plan::compile(definition.clone(), repository.clone()).unwrap();
    let mut changed = definition.clone();
    changed.inputs.task = "different task".into();
    assert_ne!(
        plan.digest,
        Plan::compile(changed, repository.clone()).unwrap().digest
    );
    let mut bad = definition.clone();
    bad.inputs.base_revision = "main".into();
    assert!(bad.validate().is_err());
    let mut bad = definition.clone();
    bad.steps.get_mut("code").unwrap().recovery_policy = Recovery::ResumeFromCheckpoint;
    assert!(bad.validate().is_err());
    let mut bad = definition.clone();
    bad.steps
        .get_mut("test")
        .unwrap()
        .commands
        .as_mut()
        .unwrap()[0]
        .cwd = "../escape".into();
    assert!(bad.validate().is_err());
    let mut bad = definition.clone();
    bad.steps.get_mut("test").unwrap().needs = Some(vec!["test".into()]);
    assert!(bad.validate().is_err());
    let mut bad = serde_json::to_value(definition).unwrap();
    bad["unexpected"] = true.into();
    assert!(serde_json::from_value::<Definition>(bad).is_err());
    let mut forbidden = repository;
    forbidden.allowed_test_executables.clear();
    assert!(Plan::compile(plan.definition, forbidden).is_err());
}
#[test]
fn operation_wire_format_roundtrips() {
    let op = Operation {
        request_id: id(),
        run_id: id(),
        attempt_id: id(),
        generation: 1,
        lease_token: id(),
        action: Action::Complete {
            success: true,
            outputs: vec![],
            failure: None,
        },
    };
    let value = serde_json::to_value(&op).unwrap();
    let decoded: Operation = serde_json::from_value(value).unwrap();
    assert_eq!(decoded.request_id, op.request_id);
}
