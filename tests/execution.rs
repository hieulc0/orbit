use anyhow::Result;
use orbit::{
    agent::{CallReceipt, CallReservation, Usage},
    execution::{Isolation, WorkerConfig},
    model::*,
    repository::{Credential, Purpose, Remote},
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn fixture() -> Result<(Definition, RepositoryBinding, WorkerConfig)> {
    let definition = Definition::parse(
        &include_str!("../examples/remote-coding.yaml")
            .replace("REPLACE_WITH_FULL_COMMIT_ID", &"a".repeat(40)),
    )?;
    let mut repository = RepositoryBinding::none();
    repository.path = "/tmp/approved-fixture".into();
    repository.coding_command = CommandSpec {
        argv: vec!["true".into()],
        cwd: ".".into(),
        timeout_seconds: 30,
    };
    repository.allowed_test_executables = vec!["sh".into()];
    let worker = serde_json::from_str(include_str!("../examples/remote-worker.json"))?;
    Ok((definition, repository, worker))
}

#[test]
fn execution_requirements_pin_profiles_preserve_legacy_and_reject_downgrades() -> Result<()> {
    let (definition, repository, worker) = fixture()?;
    worker.validate()?;
    let agent = worker.coding_agent.as_ref().unwrap();
    let server: orbit::api::Config =
        serde_json::from_str(include_str!("../examples/server-remote-coding.json"))?;
    assert_eq!(
        serde_json::to_value(&server.agent_bindings[&agent.binding_name])?,
        serde_json::to_value(&agent.binding)?
    );
    assert_eq!(
        server.execution_profiles[&Isolation::Trusted],
        worker.profiles[0]
    );
    Plan::compile_with_execution(
        definition.clone(),
        server.repositories["approved-repository"].clone(),
        &server.agent_bindings,
        &server.execution_profiles,
    )?;
    let bindings = BTreeMap::from([(agent.binding_name.clone(), agent.binding.clone())]);
    let profiles = BTreeMap::from([(Isolation::Trusted, worker.profiles[0].clone())]);
    let plan =
        Plan::compile_with_execution(definition.clone(), repository.clone(), &bindings, &profiles)?;
    assert_eq!(plan.clone().in_scope(None)?.digest, plan.digest);
    let mut changed = profiles.clone();
    changed.get_mut(&Isolation::Trusted).unwrap().image =
        format!("image@sha256:{}", "b".repeat(64));
    assert_ne!(
        plan.digest,
        Plan::compile_with_execution(definition.clone(), repository.clone(), &bindings, &changed)?
            .digest
    );
    for isolation in [Isolation::Sandboxed, Isolation::Untrusted] {
        let mut changed = definition.clone();
        changed
            .steps
            .get_mut("code")
            .unwrap()
            .execution
            .as_mut()
            .unwrap()
            .isolation = isolation.clone();
        let profiles = BTreeMap::from([(isolation, worker.profiles[0].clone())]);
        assert!(
            Plan::compile_with_execution(changed, repository.clone(), &bindings, &profiles)
                .is_err()
        );
    }
    let mut raw = serde_json::to_value(&definition)?;
    raw["steps"]["code"]["execution"]["runtime"] = json!("firecracker");
    assert!(serde_json::from_value::<Definition>(raw).is_err());
    let legacy = Definition::parse(include_str!("../examples/container.yaml"))?;
    let legacy_repository = RepositoryBinding::none();
    let expected = digest(&serde_json::to_vec(&(&legacy, &legacy_repository))?);
    let plan =
        Plan::compile_with_execution(legacy, legacy_repository, &BTreeMap::new(), &profiles)?;
    assert_eq!(plan.digest, expected);
    let value = serde_json::to_value(plan)?;
    assert!(value.get("execution_profiles").is_none());
    assert!(value["repository"].get("remote").is_none());
    Ok(())
}

#[test]
fn remote_sources_and_credentials_reject_cross_binding_audience_and_scope() -> Result<()> {
    for url in [
        "file:///tmp/repo",
        "ssh://git@example.invalid/repo",
        "https://user:secret@example.invalid/repo",
        "https://example.invalid/repo?token=x",
        "http://example.invalid/repo",
    ] {
        assert!(
            Remote {
                url: url.into(),
                credential: None,
                allow_http_loopback: true
            }
            .validate()
            .is_err()
        );
    }
    Remote {
        url: "https://example.invalid/repo.git".into(),
        credential: Some("git-read".into()),
        allow_http_loopback: false,
    }
    .validate()?;
    let (definition, repository, worker) = fixture()?;
    let agent = worker.coding_agent.as_ref().unwrap();
    let plan = Plan::compile_with_execution(
        definition,
        repository,
        &BTreeMap::from([(agent.binding_name.clone(), agent.binding.clone())]),
        &BTreeMap::from([(Isolation::Trusted, worker.profiles[0].clone())]),
    )?;
    let mut a: Assignment = serde_json::from_value(
        json!({"run_id":id(),"task_id":id(),"attempt_id":id(),"generation":1,
        "workspace_id":id(),"lease_token":"unused","lease_expires_at":0,"heartbeat_interval":100,"deadline_at":0,
        "plan":plan,"step":"code","input_artifacts":[],"idempotency_key":id()}),
    )?;
    let root = tempfile::tempdir()?;
    let path = root.path().join("credential");
    std::fs::write(&path, "fixture-private-git-credential-0000")?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let credential: Credential = serde_json::from_value(
        json!({"secret":{"provider":"file","path":path},
        "purpose":"repository","binding":"approved-repository","audience":"https://example.invalid/repo.git"}),
    )?;
    assert!(
        credential
            .resolve(
                Purpose::Repository,
                "approved-repository",
                "https://example.invalid/repo.git",
                &a
            )
            .is_ok()
    );
    assert!(
        credential
            .resolve(
                Purpose::Model,
                "approved-repository",
                "https://example.invalid/repo.git",
                &a
            )
            .is_err()
    );
    assert!(
        credential
            .resolve(
                Purpose::Repository,
                "another",
                "https://example.invalid/repo.git",
                &a
            )
            .is_err()
    );
    assert!(
        credential
            .resolve(
                Purpose::Repository,
                "approved-repository",
                "https://other.invalid/repo.git",
                &a
            )
            .is_err()
    );
    a.plan.scope = Some(orbit::governance::Scope::parse("org/project/env")?);
    assert!(
        credential
            .resolve(
                Purpose::Repository,
                "approved-repository",
                "https://example.invalid/repo.git",
                &a
            )
            .is_err()
    );
    Ok(())
}

#[test]
fn coding_tools_are_bounded_and_tracked_dispatch_is_not_replay_permission() -> Result<()> {
    let (definition, _, worker) = fixture()?;
    let agent = worker.coding_agent.as_ref().unwrap();
    agent.validate()?;
    for args in [
        json!({"path":"/etc/passwd"}),
        json!({"path":"../outside"}),
        json!({"path":"a","extra":"b"}),
    ] {
        assert!(orbit::coding_agent::tool_command("read_file", &args, 30).is_err());
    }
    assert!(orbit::coding_agent::tool_command("network", &json!({}), 30).is_err());
    let command = orbit::coding_agent::tool_command(
        "write_file",
        &json!({"path":"file","content":"$(do not expand)"}),
        30,
    )?;
    assert_eq!(command.argv.last().unwrap(), "$(do not expand)");
    let attempt = id();
    let call = CallReservation {
        call_id: format!("{attempt}-model-0"),
        tokens: 73728,
        cost_microusd: 1,
        tool: None,
        permissions: vec![],
        request_digest: Some(digest(b"request")),
    };
    let spec = definition.steps["code"].agent.as_ref().unwrap();
    let mut usage = Usage::default();
    assert!(usage.reserve(spec, &call)?);
    assert!(!usage.reserve(spec, &call)?);
    assert!(usage.pending_model_call());
    let receipt = CallReceipt {
        call_id: call.call_id,
        attempt_id: attempt,
        result_digest: digest(b"response"),
        external_id: Some("resp_fixture".into()),
    };
    assert!(usage.finish(&receipt)?);
    assert!(!usage.pending_model_call());
    assert!(!usage.finish(&receipt)?);
    let mut conflict = receipt;
    conflict.result_digest = digest(b"different");
    assert!(usage.finish(&conflict).is_err());
    assert_eq!(usage.tokens, 73728);
    let mut raw: Value = serde_json::from_str(include_str!("../examples/remote-worker.json"))?;
    raw["coding_agent"]["tokens_per_call"] = json!(1);
    assert!(
        serde_json::from_value::<WorkerConfig>(raw)?
            .validate()
            .is_err()
    );
    Ok(())
}
