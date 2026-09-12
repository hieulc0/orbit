use orbit::{compute::Resources, governance::*, model::*};
use serde_json::json;
use std::{
    collections::BTreeMap,
    os::unix::fs::{PermissionsExt, symlink},
};

fn scope(name: &str) -> Scope {
    Scope::parse(&format!("org/{name}/dev")).unwrap()
}
#[test]
fn resource_roles_are_scoped_and_global_grants_are_explicit() {
    let governance = Governance {
        roles: BTreeMap::from([("reader".into(), vec!["run.read".into()])]),
        ..Default::default()
    };
    let principal: Principal = serde_json::from_value(json!({"kind":"service_account","credential":{"provider":"env","name":"UNUSED"},"grants":[{"scope":scope("a"),"roles":["reader"]}]})).unwrap();
    assert!(governance.allows(&principal, "run.read", Some(&scope("a"))));
    assert!(!governance.allows(&principal, "run.read", Some(&scope("b"))));
    assert!(!governance.allows(&principal, "run.read", None));
    assert!(!governance.allows(&principal, "run.cancel", Some(&scope("a"))));
}
#[test]
fn execution_scope_is_pinned_without_changing_legacy_digests() {
    let definition = Definition::parse(include_str!("../examples/container.yaml")).unwrap();
    let plan = Plan::compile(definition, RepositoryBinding::none()).unwrap();
    assert_eq!(plan.digest, plan.clone().in_scope(None).unwrap().digest);
    let a = plan.clone().in_scope(Some(scope("a"))).unwrap();
    assert_ne!(plan.digest, a.digest);
    assert_eq!(
        a.digest,
        a.clone().in_scope(Some(scope("a"))).unwrap().digest
    );
    assert_ne!(a.digest, plan.in_scope(Some(scope("b"))).unwrap().digest);
}
#[test]
fn credential_files_are_private_bounded_and_not_symlinks() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("credential");
    std::fs::write(&path, b"local-fixture-token-00000000000\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        SecretRef::File { path: path.clone() }.resolve().unwrap(),
        "local-fixture-token-00000000000"
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(SecretRef::File { path: path.clone() }.resolve().is_err());
    let link = root.path().join("link");
    symlink(path, &link).unwrap();
    assert!(SecretRef::File { path: link }.resolve().is_err());
}
#[test]
fn environment_policy_restricts_nested_capabilities_and_resources() {
    let definition = Definition::parse(include_str!("../examples/container.yaml")).unwrap();
    let mut policy = Policy {
        capabilities: vec!["container.run".into()],
        repository_ids: vec![],
        agent_bindings: vec![],
        max_resources: Resources {
            cpu_millis: 1000,
            memory_mib: 128,
            gpu: 0,
        },
        max_agent_budget: None,
        max_concurrency: 8,
    };
    policy.validate_definition(&definition).unwrap();
    policy.max_resources.cpu_millis = 100;
    assert!(policy.validate_definition(&definition).is_err());
    policy.max_resources.cpu_millis = 1000;
    policy.capabilities.clear();
    assert!(policy.validate_definition(&definition).is_err());
    assert!(Scope::parse("../project/dev").is_err());
    assert!(Scope::parse("org/project/dev/extra").is_err());
}
