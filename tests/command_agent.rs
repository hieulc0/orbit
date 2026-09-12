use orbit::command_agent::CommandAgent;
use serde_json::{Value, json};

#[test]
fn command_runtime_restricts_environment_tools_and_reservations() {
    let original: Value =
        serde_json::from_str(include_str!("../examples/command-agent-runtime.json")).unwrap();
    serde_json::from_value::<CommandAgent>(original.clone())
        .unwrap()
        .validate()
        .unwrap();
    for (field, value) in [
        ("tokens_per_call", json!(0)),
        ("tokens_per_call", json!(1001)),
        ("cost_microusd_per_call", json!(1)),
    ] {
        let mut bad = original.clone();
        bad[field] = value;
        assert!(
            serde_json::from_value::<CommandAgent>(bad)
                .unwrap()
                .validate()
                .is_err()
        );
    }
    for key in [
        "ORBIT_TOKEN",
        "ORBIT_AGENT_INPUT",
        "HOME",
        "PATH",
        "LD_PRELOAD",
        "NODE_OPTIONS",
        "invalid-name",
    ] {
        let mut bad = original.clone();
        bad["environment"] = json!({key:{"provider":"env","name":"PROVIDER_CREDENTIAL"}});
        assert!(
            serde_json::from_value::<CommandAgent>(bad)
                .unwrap()
                .validate()
                .is_err()
        );
    }
    let mut bad = original.clone();
    bad["binding"]["tools"] = json!({"shell":"revision-1"});
    assert!(
        serde_json::from_value::<CommandAgent>(bad)
            .unwrap()
            .validate()
            .is_err()
    );
    let mut bad = original;
    bad["command"]["argv"] = json!(["python3"]);
    assert!(
        serde_json::from_value::<CommandAgent>(bad)
            .unwrap()
            .validate()
            .is_err()
    );
}
