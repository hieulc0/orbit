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

#[test]
fn graph_validation_and_binding_permissions() {
    let mut graph = definition();
    graph.api_version = "orbit/v1".into();
    let code = graph.steps.remove("code").unwrap();
    let mut test = graph.steps.remove("test").unwrap();
    test.needs = Some(vec!["z-code".into()]);
    graph.steps.insert("z-code".into(), code.clone());
    graph.steps.insert("a-test".into(), test.clone());
    graph.steps.insert("b-test".into(), test);
    let mut join = code;
    join.uses = "engine.join".into();
    join.needs = Some(vec!["a-test".into(), "b-test".into()]);
    graph.steps.insert("0-join".into(), join);
    graph.validate().unwrap();
    let repository = RepositoryBinding {
        path: "/tmp/fixture".into(),
        coding_command: CommandSpec {
            argv: vec!["agent".into()],
            cwd: ".".into(),
            timeout_seconds: 60,
        },
        allowed_test_executables: vec!["sh".into()],
    };
    let plan = Plan::compile(graph.clone(), repository.clone()).unwrap();
    assert_eq!(Run::new(plan, None).tasks.len(), 4);
    for needs in [
        vec!["missing"],
        vec!["z-code", "z-code"],
        vec!["a-test"],
        vec![],
    ] {
        let mut bad = graph.clone();
        bad.steps.get_mut("a-test").unwrap().needs =
            Some(needs.into_iter().map(String::from).collect());
        assert!(bad.validate().is_err());
    }
    let mut cycle = graph.clone();
    cycle.steps.get_mut("z-code").unwrap().needs = Some(vec!["0-join".into()]);
    assert!(cycle.validate().is_err());
    let mut forbidden = graph.clone();
    forbidden
        .steps
        .get_mut("b-test")
        .unwrap()
        .commands
        .as_mut()
        .unwrap()[0]
        .argv[0] = "forbidden".into();
    assert!(Plan::compile(forbidden, repository).is_err());
    graph.api_version = "orbit/v0".into();
    assert!(graph.validate().is_err());
}

#[test]
fn durable_interaction_definitions_and_legacy_serialization() {
    Definition::parse(
        &include_str!("../examples/wait-and-resume.yaml")
            .replace("REPLACE_WITH_FULL_COMMIT_ID", &"a".repeat(40)),
    )
    .unwrap();
    let legacy = definition();
    assert!(
        serde_json::to_value(&legacy).unwrap()["steps"]["code"]
            .get("delay_seconds")
            .is_none()
    );
    let mut graph = legacy.clone();
    graph.api_version = "orbit/v1".into();
    let mut timer = graph.steps.remove("code").unwrap();
    graph.steps.clear();
    timer.uses = "engine.timer".into();
    timer.delay_seconds = Some(1);
    graph.steps.insert("delay".into(), timer);
    graph.validate().unwrap();
    for delay in [None, Some(0), Some(604801), Some(u64::MAX)] {
        let mut invalid = graph.clone();
        invalid.steps.get_mut("delay").unwrap().delay_seconds = delay;
        assert!(invalid.validate().is_err());
    }
    let mut wait = graph.steps.remove("delay").unwrap();
    wait.uses = "engine.wait".into();
    wait.delay_seconds = None;
    graph.steps.insert("resume".into(), wait);
    graph.validate().unwrap();
    graph.steps.get_mut("resume").unwrap().delay_seconds = Some(1);
    assert!(graph.validate().is_err());
    let mut invalid = legacy;
    invalid.steps.get_mut("code").unwrap().delay_seconds = Some(1);
    assert!(invalid.validate().is_err());
}

#[test]
fn child_templates_and_execution_tree_bounds_are_validated() {
    for example in [
        include_str!("../examples/child-definition.yaml"),
        include_str!("../examples/fan-out.yaml"),
    ] {
        Definition::parse(&example.replace("REPLACE_WITH_FULL_COMMIT_ID", &"a".repeat(40)))
            .unwrap();
    }
    let child = definition();
    let mut parent = child.clone();
    parent.api_version = "orbit/v1".into();
    parent.steps.clear();
    let mut step = child.steps["code"].clone();
    step.uses = "engine.fan_out".into();
    step.definition = Some(Box::new(child));
    step.fan_out = Some(FanOut {
        max_items: 4,
        max_parallel: 2,
        items: Some(vec!["first".into(), "second".into()]),
        signal_from: None,
        agent_from: None,
    });
    parent.steps.insert("dispatch".into(), step);
    parent.max_concurrency = Some(2);
    parent.validate().unwrap();
    let repository = RepositoryBinding {
        path: "/tmp/fixture".into(),
        coding_command: CommandSpec {
            argv: vec!["agent".into()],
            cwd: ".".into(),
            timeout_seconds: 60,
        },
        allowed_test_executables: vec!["sh".into()],
    };
    let original = Plan::compile(parent.clone(), repository.clone()).unwrap();
    let mut changed = parent.clone();
    changed
        .steps
        .get_mut("dispatch")
        .unwrap()
        .definition
        .as_mut()
        .unwrap()
        .inputs
        .task = "changed child".into();
    assert_ne!(
        original.digest,
        Plan::compile(changed, repository.clone()).unwrap().digest
    );
    let mut forbidden = parent.clone();
    forbidden
        .steps
        .get_mut("dispatch")
        .unwrap()
        .definition
        .as_mut()
        .unwrap()
        .steps
        .get_mut("test")
        .unwrap()
        .commands
        .as_mut()
        .unwrap()[0]
        .argv[0] = "forbidden".into();
    assert!(Plan::compile(forbidden, repository).is_err());
    let mut foreign = parent.clone();
    foreign
        .steps
        .get_mut("dispatch")
        .unwrap()
        .definition
        .as_mut()
        .unwrap()
        .inputs
        .repository_id = "other".into();
    assert!(foreign.validate().is_err());
    for (max_items, max_parallel) in [(0, 1), (65, 1), (4, 0), (4, 5)] {
        let mut invalid = parent.clone();
        let fan = invalid
            .steps
            .get_mut("dispatch")
            .unwrap()
            .fan_out
            .as_mut()
            .unwrap();
        fan.max_items = max_items;
        fan.max_parallel = max_parallel;
        assert!(invalid.validate().is_err());
    }
    let mut missing = parent.clone();
    missing.steps.get_mut("dispatch").unwrap().definition = None;
    assert!(missing.validate().is_err());
    let mut source = parent.clone();
    let fan = source
        .steps
        .get_mut("dispatch")
        .unwrap()
        .fan_out
        .as_mut()
        .unwrap();
    fan.items = None;
    fan.signal_from = Some("missing".into());
    assert!(source.validate().is_err());
    let mut nested = parent.clone();
    for _ in 0..4 {
        let mut outer = parent.clone();
        outer.steps.get_mut("dispatch").unwrap().definition = Some(Box::new(nested));
        nested = outer;
    }
    assert!(nested.validate().is_err());
    let mut too_wide = parent.clone();
    too_wide
        .steps
        .get_mut("dispatch")
        .unwrap()
        .fan_out
        .as_mut()
        .unwrap()
        .max_items = 64;
    let mut outer = too_wide.clone();
    outer.steps.get_mut("dispatch").unwrap().definition = Some(Box::new(too_wide));
    assert!(outer.validate().is_err());
    parent.max_concurrency = Some(0);
    assert!(parent.validate().is_err());
    assert!(
        Limits {
            max_active_roots: 0,
            ..Limits::default()
        }
        .validate()
        .is_err()
    );
}
